use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fmt, str::FromStr};
use wmcterm_core::{AgentState, AttentionState};

/// Stable identity used to prove that an event came from the expected agent
/// process rather than a reused PID or an unrelated local process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProcessIdentity {
    pub pid: u32,
    /// Linux `/proc/<pid>/stat` field 22, measured in clock ticks since boot.
    pub pid_start_time: u64,
    pub surface_id: String,
    pub lifecycle_id: String,
    /// Raw Linux `tty_nr` value. `None` means the process has no controlling tty
    /// or that a caller does not require tty matching during validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controlling_tty: Option<i64>,
}

impl AgentProcessIdentity {
    pub fn expected(
        pid: u32,
        pid_start_time: u64,
        surface_id: impl Into<String>,
        lifecycle_id: impl Into<String>,
    ) -> Self {
        Self {
            pid,
            pid_start_time,
            surface_id: surface_id.into(),
            lifecycle_id: lifecycle_id.into(),
            controlling_tty: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessIdentityError {
    #[error("agent process identity is unsupported on this platform")]
    Unsupported,
    #[error("invalid process id {0}")]
    InvalidPid(u32),
    #[error("process {pid} is unavailable ({kind:?})")]
    Unavailable { pid: u32, kind: std::io::ErrorKind },
    #[error("process {pid} {resource} exceeded the safe read limit")]
    ResourceTooLarge { pid: u32, resource: &'static str },
    #[error("process {pid} has malformed {resource}")]
    Malformed { pid: u32, resource: &'static str },
    #[error("process {pid} is missing {variable}")]
    MissingEnvironment { pid: u32, variable: &'static str },
    #[error("process {pid} has an ambiguous {variable}")]
    AmbiguousEnvironment { pid: u32, variable: &'static str },
    #[error("process {pid} changed while its identity was being read")]
    ProcessChanged { pid: u32 },
    #[error("process {pid} start time does not match (possible PID reuse)")]
    PidReused { pid: u32 },
    #[error("process {pid} {field} does not match the expected terminal identity")]
    IdentityMismatch { pid: u32, field: &'static str },
}

/// Read a process identity from the platform process table.
///
/// Linux requires both wmcterm routing variables. Missing permissions, missing
/// variables, malformed proc files, and process races all fail closed.
pub fn read_agent_process_identity(pid: u32) -> Result<AgentProcessIdentity, ProcessIdentityError> {
    platform_identity::read(pid)
}

/// Re-read and validate a previously recorded identity. A successful result is
/// the freshly observed identity, suitable for attaching to the accepted event.
pub fn validate_agent_process_identity(
    expected: &AgentProcessIdentity,
) -> Result<AgentProcessIdentity, ProcessIdentityError> {
    let observed = read_agent_process_identity(expected.pid)?;
    compare_agent_process_identity(expected, &observed)?;
    Ok(observed)
}

fn compare_agent_process_identity(
    expected: &AgentProcessIdentity,
    observed: &AgentProcessIdentity,
) -> Result<(), ProcessIdentityError> {
    if observed.pid_start_time != expected.pid_start_time {
        return Err(ProcessIdentityError::PidReused { pid: expected.pid });
    }
    if observed.surface_id != expected.surface_id {
        return Err(ProcessIdentityError::IdentityMismatch {
            pid: expected.pid,
            field: "surface id",
        });
    }
    if observed.lifecycle_id != expected.lifecycle_id {
        return Err(ProcessIdentityError::IdentityMismatch {
            pid: expected.pid,
            field: "lifecycle id",
        });
    }
    if expected.controlling_tty.is_some() && observed.controlling_tty != expected.controlling_tty {
        return Err(ProcessIdentityError::IdentityMismatch {
            pid: expected.pid,
            field: "controlling tty",
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod platform_identity {
    use std::{
        fs::File,
        io::{Read, Take},
        path::{Path, PathBuf},
    };

    use super::{AgentProcessIdentity, ProcessIdentityError};

    const STAT_LIMIT: u64 = 64 * 1024;
    const ENVIRON_LIMIT: u64 = 1024 * 1024;
    const SURFACE: &str = "WMCTERM_SURFACE_ID";
    const LIFECYCLE: &str = "WMCTERM_TERMINAL_LIFECYCLE_ID";

    pub(super) fn read(pid: u32) -> Result<AgentProcessIdentity, ProcessIdentityError> {
        if pid == 0 {
            return Err(ProcessIdentityError::InvalidPid(pid));
        }
        let root = PathBuf::from("/proc").join(pid.to_string());
        let first = read_stat(pid, &root.join("stat"))?;
        let environ = read_bounded(pid, &root.join("environ"), "environment", ENVIRON_LIMIT)?;
        let (surface_id, lifecycle_id) = parse_environment(pid, &environ)?;
        let second = read_stat(pid, &root.join("stat"))?;
        if first.0 != second.0 {
            return Err(ProcessIdentityError::ProcessChanged { pid });
        }
        Ok(AgentProcessIdentity {
            pid,
            pid_start_time: second.0,
            surface_id,
            lifecycle_id,
            controlling_tty: second.1,
        })
    }

    fn read_stat(pid: u32, path: &Path) -> Result<(u64, Option<i64>), ProcessIdentityError> {
        let bytes = read_bounded(pid, path, "stat", STAT_LIMIT)?;
        let stat = std::str::from_utf8(&bytes).map_err(|_| ProcessIdentityError::Malformed {
            pid,
            resource: "stat",
        })?;
        parse_stat(pid, stat)
    }

    fn parse_stat(pid: u32, stat: &str) -> Result<(u64, Option<i64>), ProcessIdentityError> {
        // comm is parenthesized and may contain spaces or ')' characters, so
        // split after the final ')' rather than using whitespace for fields 1/2.
        let close = stat.rfind(')').ok_or(ProcessIdentityError::Malformed {
            pid,
            resource: "stat",
        })?;
        let prefix = stat[..close].trim_start();
        let (parsed_pid, _) = prefix
            .split_once(' ')
            .ok_or(ProcessIdentityError::Malformed {
                pid,
                resource: "stat",
            })?;
        if parsed_pid.parse::<u32>().ok() != Some(pid) {
            return Err(ProcessIdentityError::Malformed {
                pid,
                resource: "stat",
            });
        }
        let fields: Vec<_> = stat[close + 1..].split_whitespace().collect();
        let tty = fields
            .get(4)
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or(ProcessIdentityError::Malformed {
                pid,
                resource: "stat",
            })?;
        let start_time = fields
            .get(19)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(ProcessIdentityError::Malformed {
                pid,
                resource: "stat",
            })?;
        Ok((start_time, (tty != 0).then_some(tty)))
    }

    fn parse_environment(
        pid: u32,
        environ: &[u8],
    ) -> Result<(String, String), ProcessIdentityError> {
        let surface = unique_environment(pid, environ, SURFACE)?;
        let lifecycle = unique_environment(pid, environ, LIFECYCLE)?;
        Ok((surface, lifecycle))
    }

    fn unique_environment(
        pid: u32,
        environ: &[u8],
        name: &'static str,
    ) -> Result<String, ProcessIdentityError> {
        let prefix = format!("{name}=").into_bytes();
        let mut values = environ
            .split(|byte| *byte == 0)
            .filter_map(|entry| entry.strip_prefix(prefix.as_slice()));
        let value = values
            .next()
            .ok_or(ProcessIdentityError::MissingEnvironment {
                pid,
                variable: name,
            })?;
        if values.next().is_some() {
            return Err(ProcessIdentityError::AmbiguousEnvironment {
                pid,
                variable: name,
            });
        }
        let value = std::str::from_utf8(value).map_err(|_| ProcessIdentityError::Malformed {
            pid,
            resource: "environment",
        })?;
        if value.trim().is_empty() || value.len() > 256 {
            return Err(ProcessIdentityError::Malformed {
                pid,
                resource: "environment",
            });
        }
        Ok(value.to_owned())
    }

    fn read_bounded(
        pid: u32,
        path: &Path,
        resource: &'static str,
        limit: u64,
    ) -> Result<Vec<u8>, ProcessIdentityError> {
        let file = File::open(path).map_err(|error| ProcessIdentityError::Unavailable {
            pid,
            kind: error.kind(),
        })?;
        let mut reader: Take<File> = file.take(limit + 1);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| ProcessIdentityError::Unavailable {
                pid,
                kind: error.kind(),
            })?;
        if bytes.len() as u64 > limit {
            return Err(ProcessIdentityError::ResourceTooLarge { pid, resource });
        }
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn stat_fixture(pid: u32, tty: i64, start: u64) -> String {
            // fields after comm are 3..22; the name intentionally contains ')'.
            format!(
                "{pid} (agent ) worker) S 1 2 3 {tty} 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start}"
            )
        }

        #[test]
        fn parses_proc_fixture_with_hostile_comm_and_identity_environment() {
            assert_eq!(
                parse_stat(41, &stat_fixture(41, 34817, 9988)).unwrap(),
                (9988, Some(34817))
            );
            let environment =
                b"PATH=/bin\0WMCTERM_SURFACE_ID=surface-a\0WMCTERM_TERMINAL_LIFECYCLE_ID=life-a\0";
            assert_eq!(
                parse_environment(41, environment).unwrap(),
                ("surface-a".into(), "life-a".into())
            );
        }

        #[test]
        fn fixtures_fail_closed_for_bad_stat_and_ambiguous_or_missing_binding() {
            assert!(matches!(
                parse_stat(41, "41 malformed"),
                Err(ProcessIdentityError::Malformed { .. })
            ));
            let duplicate =
                b"WMCTERM_SURFACE_ID=a\0WMCTERM_SURFACE_ID=b\0WMCTERM_TERMINAL_LIFECYCLE_ID=c\0";
            assert!(matches!(
                parse_environment(41, duplicate),
                Err(ProcessIdentityError::AmbiguousEnvironment {
                    variable: SURFACE,
                    ..
                })
            ));
            assert!(matches!(
                parse_environment(41, b"WMCTERM_SURFACE_ID=a\0"),
                Err(ProcessIdentityError::MissingEnvironment {
                    variable: LIFECYCLE,
                    ..
                })
            ));
        }

        #[test]
        fn reads_the_current_process_stat_and_rejects_a_missing_process() {
            let pid = std::process::id();
            let (start, _) = read_stat(pid, &PathBuf::from("/proc/self/stat")).unwrap();
            assert!(start > 0);
            assert!(matches!(
                read(u32::MAX),
                Err(ProcessIdentityError::Unavailable { .. })
            ));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform_identity {
    use super::{AgentProcessIdentity, ProcessIdentityError};

    pub(super) fn read(_pid: u32) -> Result<AgentProcessIdentity, ProcessIdentityError> {
        Err(ProcessIdentityError::Unsupported)
    }
}

/// AI agents with a hook/event protocol understood by wmcterm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Grok,
    Pi,
    Omp,
    Amp,
}

impl AgentKind {
    pub const ALL: [Self; 8] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Gemini,
        Self::Grok,
        Self::Pi,
        Self::Omp,
        Self::Amp,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Grok => "grok",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Amp => "amp",
        }
    }

    /// Detect a supported agent from a process command line or executable
    /// name without matching arbitrary substrings.
    pub fn from_process_command(command: &str) -> Option<Self> {
        let normalized = command.to_ascii_lowercase().replace('\\', "/");
        for word in normalized
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, '/' | ':' | '=')
            })
            .filter(|word| !word.is_empty())
        {
            let stem = word
                .strip_suffix(".exe")
                .unwrap_or(word)
                .trim_matches(|character: char| matches!(character, '\'' | '"'));
            let kind = if stem == "codex" || stem.starts_with("codex-") {
                Some(Self::Codex)
            } else if matches!(stem, "claude" | "claude-code" | "@anthropic-ai") {
                Some(Self::Claude)
            } else if matches!(stem, "opencode" | "open-code") {
                Some(Self::OpenCode)
            } else if matches!(stem, "gemini" | "gemini-cli" | "@google") {
                Some(Self::Gemini)
            } else if matches!(stem, "grok" | "grok-cli") {
                Some(Self::Grok)
            } else if matches!(stem, "pi" | "pi-coding-agent") {
                Some(Self::Pi)
            } else if matches!(stem, "omp" | "oh-my-pi") {
                Some(Self::Omp)
            } else if matches!(stem, "amp" | "amp-code") {
                Some(Self::Amp)
            } else {
                None
            };
            if kind.is_some() {
                return kind;
            }
        }
        None
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unsupported agent kind: {0}")]
pub struct ParseAgentKindError(pub String);

impl FromStr for AgentKind {
    type Err = ParseAgentKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match canonical(value).as_str() {
            "claude" | "claudecode" => Ok(Self::Claude),
            "codex" | "openaicodex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "gemini" | "geminicli" => Ok(Self::Gemini),
            "grok" | "grokcli" => Ok(Self::Grok),
            "pi" | "picodingagent" => Ok(Self::Pi),
            "omp" | "ohmypi" => Ok(Self::Omp),
            "amp" | "ampcode" => Ok(Self::Amp),
            _ => Err(ParseAgentKindError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    SessionStart {
        agent: String,
        session_id: Option<String>,
    },
    ToolUse {
        tool: String,
    },
    PermissionRequest {
        message: String,
    },
    NeedsInput {
        message: String,
    },
    TurnComplete {
        continuing: bool,
        message: Option<String>,
    },
    Stop {
        success: bool,
        message: Option<String>,
    },
}

/// Codex subagent lifecycle data from the native 0.151 hook protocol.
///
/// These identities are intentionally read only from top-level native fields.
/// A nested or incomplete payload must not be routed to another terminal's
/// parent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSubagentHookEvent {
    pub kind: CodexSubagentHookKind,
    pub session_id: String,
    pub turn_id: String,
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexSubagentHookKind {
    Start,
    Stop,
}

impl CodexSubagentHookEvent {
    pub fn validate(&self) -> Result<(), NormalizeHookError> {
        let event = match self.kind {
            CodexSubagentHookKind::Start => "SubagentStart",
            CodexSubagentHookKind::Stop => "SubagentStop",
        };
        for (field, value) in [
            ("session_id", self.session_id.as_str()),
            ("turn_id", self.turn_id.as_str()),
            ("agent_id", self.agent_id.as_str()),
            ("agent_type", self.agent_type.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(NormalizeHookError::MissingIdentity {
                    agent: AgentKind::Codex,
                    event: event.to_owned(),
                    field,
                });
            }
        }
        Ok(())
    }

    pub fn route_key(&self) -> (&str, &str) {
        (&self.session_id, &self.agent_id)
    }

    /// Compatibility projection for lifecycle consumers that do not track
    /// subagents. Both events are activity; neither completes the parent turn.
    pub fn to_agent_event(&self) -> AgentEvent {
        let action = match self.kind {
            CodexSubagentHookKind::Start => "started",
            CodexSubagentHookKind::Stop => "stopped",
        };
        AgentEvent::ToolUse {
            tool: format!("Codex subagent {action}: {}", self.agent_type),
        }
    }
}

/// Strictly normalize Codex 0.151 `SubagentStart` and `SubagentStop` payloads.
pub fn normalize_codex_subagent_hook_event(
    event_name: Option<&str>,
    payload: &Value,
) -> Result<CodexSubagentHookEvent, NormalizeHookError> {
    if !payload.is_object() {
        return Err(NormalizeHookError::InvalidPayload);
    }
    let payload_event = direct_string(payload, "hook_event_name");
    if let (Some(argument), Some(payload_event)) = (event_name, payload_event.as_deref())
        && argument != payload_event
    {
        return Err(NormalizeHookError::EventNameMismatch {
            agent: AgentKind::Codex,
            argument: argument.to_owned(),
            payload: payload_event.to_owned(),
        });
    }
    let raw_event = event_name
        .map(str::to_owned)
        .or(payload_event)
        .ok_or(NormalizeHookError::MissingEventName)?;
    let kind = match raw_event.as_str() {
        "SubagentStart" => CodexSubagentHookKind::Start,
        "SubagentStop" => CodexSubagentHookKind::Stop,
        _ => {
            return Err(NormalizeHookError::UnsupportedEvent {
                agent: AgentKind::Codex,
                event: raw_event,
            });
        }
    };
    let event = CodexSubagentHookEvent {
        kind,
        session_id: required_hook_string(payload, "session_id", AgentKind::Codex, &raw_event)?,
        turn_id: required_hook_string(payload, "turn_id", AgentKind::Codex, &raw_event)?,
        agent_id: required_hook_string(payload, "agent_id", AgentKind::Codex, &raw_event)?,
        agent_type: required_hook_string(payload, "agent_type", AgentKind::Codex, &raw_event)?,
        agent_transcript_path: direct_string(payload, "agent_transcript_path")
            .and_then(optional_nonempty),
        last_assistant_message: direct_string(payload, "last_assistant_message")
            .and_then(optional_nonempty),
    };
    event.validate()?;
    Ok(event)
}

/// Claude Code lifecycle information that cannot be represented by the legacy
/// surface-level [`AgentEvent`] state machine.
///
/// A Claude subagent normally runs inside the parent's process and TUI.  These
/// events therefore identify work that wmcterm may *represent* in another pane;
/// they do not claim that the internal Claude TUI can be moved to that pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHookEvent {
    pub kind: ClaudeHookKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// Stable actions consumed by a future app-side Claude pane coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeHookKind {
    SessionStart,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    Notification,
    Stop,
}

/// Recommended UI coordination action. `OpenSubagentStatus` means a status
/// representation (card, virtual pane, or sidebar row), not a second PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudePaneAction {
    BindSession,
    ReleaseSession,
    OpenSubagentStatus,
    UpdateAgentStatus,
    CloseSubagentStatus,
}

impl ClaudeHookEvent {
    /// Identity stable enough for an in-memory subagent-to-pane binding.
    /// Session IDs are deliberately included because Claude may reuse short
    /// agent IDs in a later session.
    pub fn route_key(&self) -> String {
        match &self.agent_id {
            Some(agent_id) => format!("claude:{}:{agent_id}", self.session_id),
            None => format!("claude:{}", self.session_id),
        }
    }

    pub const fn pane_action(&self) -> ClaudePaneAction {
        match self.kind {
            ClaudeHookKind::SessionStart => ClaudePaneAction::BindSession,
            ClaudeHookKind::SessionEnd => ClaudePaneAction::ReleaseSession,
            ClaudeHookKind::SubagentStart => ClaudePaneAction::OpenSubagentStatus,
            ClaudeHookKind::SubagentStop => ClaudePaneAction::CloseSubagentStatus,
            ClaudeHookKind::PreToolUse
            | ClaudeHookKind::PostToolUse
            | ClaudeHookKind::PostToolUseFailure
            | ClaudeHookKind::PermissionRequest
            | ClaudeHookKind::Notification
            | ClaudeHookKind::Stop => ClaudePaneAction::UpdateAgentStatus,
        }
    }

    /// Compatibility projection for the existing surface state machine.
    /// Subagent completion is activity, not a root-session Stop; treating it as
    /// Stop would incorrectly mark every agent in the terminal as completed.
    pub fn to_agent_event(&self) -> AgentEvent {
        match self.kind {
            ClaudeHookKind::SessionStart => AgentEvent::SessionStart {
                agent: AgentKind::Claude.to_string(),
                session_id: Some(self.session_id.clone()),
            },
            ClaudeHookKind::SessionEnd => AgentEvent::Stop {
                success: true,
                message: self.message.clone().or_else(|| self.reason.clone()),
            },
            ClaudeHookKind::Stop => AgentEvent::TurnComplete {
                continuing: self.stop_hook_active,
                message: self.message.clone().or_else(|| self.reason.clone()),
            },
            ClaudeHookKind::PermissionRequest => AgentEvent::PermissionRequest {
                message: self
                    .message
                    .clone()
                    .unwrap_or_else(|| "Agent is waiting for permission".into()),
            },
            ClaudeHookKind::Notification => {
                let notification = self
                    .notification_type
                    .as_deref()
                    .map(canonical)
                    .unwrap_or_default();
                if notification.contains("permission") {
                    AgentEvent::PermissionRequest {
                        message: self
                            .message
                            .clone()
                            .unwrap_or_else(|| "Agent is waiting for permission".into()),
                    }
                } else if notification.contains("idle") || notification.contains("input") {
                    AgentEvent::NeedsInput {
                        message: self
                            .message
                            .clone()
                            .unwrap_or_else(|| "Agent is waiting for input".into()),
                    }
                } else {
                    AgentEvent::ToolUse {
                        tool: self
                            .message
                            .clone()
                            .unwrap_or_else(|| "Claude notification".into()),
                    }
                }
            }
            ClaudeHookKind::SubagentStart => AgentEvent::ToolUse {
                tool: format!(
                    "subagent started: {}",
                    self.agent_type.as_deref().unwrap_or("unknown")
                ),
            },
            ClaudeHookKind::SubagentStop => AgentEvent::ToolUse {
                tool: format!(
                    "subagent stopped: {}",
                    self.agent_type.as_deref().unwrap_or("unknown")
                ),
            },
            ClaudeHookKind::PreToolUse
            | ClaudeHookKind::PostToolUse
            | ClaudeHookKind::PostToolUseFailure => AgentEvent::ToolUse {
                tool: self
                    .tool_name
                    .clone()
                    .unwrap_or_else(|| "Claude tool".into()),
            },
        }
    }
}

/// Strictly normalize the documented Claude Code 2.1 hook payload shape.
///
/// `session_id` is required for every event and `agent_id` is additionally
/// required for subagent events.  Failing closed here prevents a coordinator
/// from attaching lifecycle events to the wrong pane.
pub fn normalize_claude_hook_event(
    event_name: Option<&str>,
    payload: &Value,
) -> Result<ClaudeHookEvent, NormalizeHookError> {
    if !payload.is_object() {
        return Err(NormalizeHookError::InvalidPayload);
    }
    let raw_event = event_name
        .map(str::to_owned)
        .or_else(|| find_string(payload, &["hook_event_name"]))
        .ok_or(NormalizeHookError::MissingEventName)?;
    let canonical_event = canonical(&raw_event);
    let kind = match canonical_event.as_str() {
        "sessionstart" => ClaudeHookKind::SessionStart,
        "sessionend" => ClaudeHookKind::SessionEnd,
        "subagentstart" => ClaudeHookKind::SubagentStart,
        "subagentstop" => ClaudeHookKind::SubagentStop,
        "pretooluse" => ClaudeHookKind::PreToolUse,
        "posttooluse" => ClaudeHookKind::PostToolUse,
        "posttoolusefailure" => ClaudeHookKind::PostToolUseFailure,
        "permissionrequest" => ClaudeHookKind::PermissionRequest,
        "notification" => ClaudeHookKind::Notification,
        "stop" => ClaudeHookKind::Stop,
        _ => {
            return Err(NormalizeHookError::UnsupportedEvent {
                agent: AgentKind::Claude,
                event: raw_event,
            });
        }
    };
    let session_id = required_hook_string(payload, "session_id", AgentKind::Claude, &raw_event)?;
    let agent_id = direct_string(payload, "agent_id");
    if matches!(
        kind,
        ClaudeHookKind::SubagentStart | ClaudeHookKind::SubagentStop
    ) && agent_id.as_deref().is_none_or(str::is_empty)
    {
        return Err(NormalizeHookError::MissingIdentity {
            agent: AgentKind::Claude,
            event: raw_event,
            field: "agent_id",
        });
    }
    Ok(ClaudeHookEvent {
        kind,
        session_id,
        agent_id,
        agent_type: direct_string(payload, "agent_type"),
        cwd: direct_string(payload, "cwd"),
        transcript_path: direct_string(payload, "transcript_path"),
        agent_transcript_path: direct_string(payload, "agent_transcript_path"),
        tool_name: direct_string(payload, "tool_name"),
        tool_use_id: direct_string(payload, "tool_use_id"),
        notification_type: direct_string(payload, "notification_type"),
        message: find_string(payload, &["message", "last_assistant_message", "error"])
            .and_then(optional_nonempty),
        reason: direct_string(payload, "reason"),
        stop_hook_active: payload
            .get("stop_hook_active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn required_hook_string(
    payload: &Value,
    field: &'static str,
    agent: AgentKind,
    event: &str,
) -> Result<String, NormalizeHookError> {
    direct_string(payload, field)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| NormalizeHookError::MissingIdentity {
            agent,
            event: event.to_owned(),
            field,
        })
}

fn direct_string(payload: &Value, key: &str) -> Option<String> {
    let value = payload.as_object()?.get(key)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| (value.is_number() || value.is_boolean()).then(|| value.to_string()))
}

/// Versioned context carried with an agent lifecycle event.
///
/// All routing/process fields are optional so an envelope can also wrap the
/// legacy `AgentEvent` accepted by `wmcterm agent-event`. The canonical wire
/// representation nests the event; [`AgentEventEnvelope::to_legacy_rpc_params`]
/// adapts it to the existing GUI RPC without requiring a synchronized upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub event: AgentEvent,
}

impl AgentEventEnvelope {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(event: AgentEvent) -> Self {
        Self {
            surface_id: None,
            lifecycle_id: None,
            agent_pid: None,
            pid_start_time: None,
            version: Some(Self::CURRENT_VERSION),
            sequence: None,
            event,
        }
    }

    /// Populate commonly emitted hook metadata, including nested payloads.
    pub fn from_hook_payload(
        event: AgentEvent,
        payload: &Value,
        surface_id: Option<String>,
    ) -> Self {
        let mut envelope = Self::new(event);
        envelope.surface_id =
            surface_id.or_else(|| find_string(payload, &["surface_id", "surfaceId", "surface"]));
        envelope.lifecycle_id = find_string(
            payload,
            &["lifecycle_id", "lifecycleId", "terminal_lifecycle_id"],
        );
        envelope.agent_pid = find_integer(payload, &["agent_pid", "agentPid", "pid"])
            .and_then(|value| u32::try_from(value).ok());
        envelope.pid_start_time = find_integer(
            payload,
            &["pid_start_time", "pidStartTime", "process_start_time"],
        );
        envelope.sequence = find_integer(payload, &["sequence", "sequence_id", "sequenceId"]);
        envelope
    }

    /// Convert the canonical envelope to parameters understood by both old
    /// and new `agent.update` handlers. Unknown metadata is ignored by the old
    /// serde representation of `AgentEvent`.
    pub fn to_legacy_rpc_params(&self) -> serde_json::Result<Value> {
        let mut params = serde_json::to_value(&self.event)?;
        let Some(object) = params.as_object_mut() else {
            return Ok(params);
        };
        if let Some(surface) = &self.surface_id {
            object.insert("surface".into(), surface.clone().into());
            object.insert("surface_id".into(), surface.clone().into());
        }
        insert_optional(object, "lifecycle_id", self.lifecycle_id.clone());
        insert_optional(object, "agent_pid", self.agent_pid);
        insert_optional(object, "pid_start_time", self.pid_start_time);
        insert_optional(object, "version", self.version);
        insert_optional(object, "sequence", self.sequence);
        Ok(params)
    }

    /// Accept either the canonical nested envelope or a legacy raw event.
    pub fn from_compatible_value(value: Value) -> serde_json::Result<Self> {
        if value.get("event").is_some_and(Value::is_object) {
            serde_json::from_value(value)
        } else {
            serde_json::from_value(value).map(Self::new)
        }
    }
}

fn insert_optional<T: Serialize>(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        object.insert(key.to_owned(), value);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NormalizeHookError {
    #[error("hook payload must be a JSON object")]
    InvalidPayload,
    #[error("hook payload does not contain an event name")]
    MissingEventName,
    #[error("{agent} hook event argument {argument} does not match payload event {payload}")]
    EventNameMismatch {
        agent: AgentKind,
        argument: String,
        payload: String,
    },
    #[error("unsupported {agent} hook event: {event}")]
    UnsupportedEvent { agent: AgentKind, event: String },
    #[error("{agent} hook event {event} is missing routing field {field}")]
    MissingIdentity {
        agent: AgentKind,
        event: String,
        field: &'static str,
    },
}

/// Normalize a native agent hook payload into wmcterm's stable event model.
///
/// `event_name` takes precedence over names embedded in the payload. This is
/// useful for hook systems (such as Claude Code) that select a command by hook
/// name while sending only event data on stdin.
pub fn normalize_hook_event(
    agent: AgentKind,
    event_name: Option<&str>,
    payload: &Value,
) -> Result<AgentEvent, NormalizeHookError> {
    if !payload.is_object() {
        return Err(NormalizeHookError::InvalidPayload);
    }
    let raw_event = event_name
        .map(str::to_owned)
        .or_else(|| {
            find_string(
                payload,
                &["hook_event_name", "event_name", "event", "type", "name"],
            )
        })
        .ok_or(NormalizeHookError::MissingEventName)?;
    let event = canonical(&raw_event);

    // Prefer Claude's strict native shape when routing identity is available.
    // The permissive rules below remain for older hook adapters that did not
    // forward session identity.
    if matches!(agent, AgentKind::Claude)
        && let Ok(event) = normalize_claude_hook_event(Some(&raw_event), payload)
    {
        return Ok(event.to_agent_event());
    }

    if matches!(agent, AgentKind::Codex)
        && matches!(raw_event.as_str(), "SubagentStart" | "SubagentStop")
    {
        return normalize_codex_subagent_hook_event(Some(&raw_event), payload)
            .map(|event| event.to_agent_event());
    }

    // Even a legacy adapter must not complete the root session when one of its
    // internal subagents stops.
    if matches!(agent, AgentKind::Claude)
        && matches!(event.as_str(), "subagentstart" | "subagentstop")
    {
        return Ok(AgentEvent::ToolUse {
            tool: format!("Claude {raw_event}"),
        });
    }
    let message = find_string(
        payload,
        &[
            "message",
            "reason",
            "prompt",
            "description",
            "summary",
            "error",
            "last_assistant_message",
            "last-assistant-message",
        ],
    )
    .unwrap_or_default();

    if is_session_start(agent, &event) {
        return Ok(AgentEvent::SessionStart {
            agent: agent.to_string(),
            session_id: find_string(
                payload,
                &["session_id", "sessionId", "thread_id", "threadId"],
            ),
        });
    }

    if is_permission(agent, &event, payload) {
        return Ok(AgentEvent::PermissionRequest {
            message: nonempty(message, "Agent is waiting for permission"),
        });
    }

    if is_needs_input(agent, &event, payload) {
        return Ok(AgentEvent::NeedsInput {
            message: nonempty(message, "Agent is waiting for input"),
        });
    }

    // Codex Interrupt ends only the active turn. The Codex session and its
    // terminal process remain alive and can accept another prompt, so this
    // must not be projected as the legacy session-level Stop event.
    if matches!(agent, AgentKind::Codex) && event == "interrupt" {
        return Ok(AgentEvent::TurnComplete {
            continuing: false,
            message: optional_nonempty(message),
        });
    }

    if is_failure(&event, payload) {
        return Ok(AgentEvent::Stop {
            success: false,
            message: optional_nonempty(message),
        });
    }

    if is_stop(agent, &event) {
        return Ok(AgentEvent::Stop {
            success: payload_success(payload).unwrap_or(true),
            message: optional_nonempty(message),
        });
    }

    if is_tool(agent, &event, payload) {
        return Ok(AgentEvent::ToolUse {
            tool: find_string(
                payload,
                &["tool_name", "toolName", "tool", "command", "item_type"],
            )
            .unwrap_or_else(|| nonempty(message, "agent activity")),
        });
    }

    Err(NormalizeHookError::UnsupportedEvent {
        agent,
        event: raw_event,
    })
}

/// Parse JSON received from a hook process and normalize it in one call.
pub fn normalize_hook_json(
    agent: AgentKind,
    event_name: Option<&str>,
    json: &str,
) -> Result<AgentEvent, HookJsonError> {
    let payload = serde_json::from_str(json)?;
    Ok(normalize_hook_event(agent, event_name, &payload)?)
}

#[derive(Debug, thiserror::Error)]
pub enum HookJsonError {
    #[error("invalid hook JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Normalize(#[from] NormalizeHookError),
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key) {
            if let Some(text) = value.as_str() {
                return Some(text.to_owned());
            }
            if value.is_number() || value.is_boolean() {
                return Some(value.to_string());
            }
        }
    }
    for key in ["data", "payload", "event", "context", "item", "tool"] {
        if let Some(found) = object.get(key).and_then(|value| find_string(value, keys)) {
            return Some(found);
        }
    }
    None
}

fn find_integer(value: &Value, keys: &[&str]) -> Option<u64> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key) {
            if let Some(integer) = value.as_u64() {
                return Some(integer);
            }
            if let Some(integer) = value.as_str().and_then(|value| value.parse().ok()) {
                return Some(integer);
            }
        }
    }
    for key in ["data", "payload", "event", "context", "process"] {
        if let Some(found) = object.get(key).and_then(|value| find_integer(value, keys)) {
            return Some(found);
        }
    }
    None
}

fn canonical(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn nonempty(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

fn optional_nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn any(event: &str, names: &[&str]) -> bool {
    names.contains(&event)
}

fn is_session_start(agent: AgentKind, event: &str) -> bool {
    any(
        event,
        &[
            "sessionstart",
            "startup",
            "initialize",
            "threadstarted",
            "agentstart",
        ],
    ) || matches!(agent, AgentKind::Codex) && event == "sessionconfigured"
        || matches!(agent, AgentKind::OpenCode) && event == "sessioncreated"
}

fn is_permission(agent: AgentKind, event: &str, payload: &Value) -> bool {
    any(
        event,
        &[
            "permissionrequest",
            "permissionrequested",
            "approvalrequest",
            "approvalrequested",
            "beforetoolapproval",
        ],
    ) || matches!(agent, AgentKind::OpenCode) && event == "permissionupdated"
        || event == "notification"
            && find_string(payload, &["notification_type", "notificationType", "kind"])
                .is_some_and(|kind| canonical(&kind).contains("permission"))
}

fn is_needs_input(agent: AgentKind, event: &str, payload: &Value) -> bool {
    any(
        event,
        &[
            "needsinput",
            "inputrequested",
            "userprompt",
            "askuser",
            "idleprompt",
        ],
    ) || matches!(agent, AgentKind::OpenCode) && event == "sessionidle"
        || matches!(agent, AgentKind::OpenCode)
            && event == "sessionstatus"
            && find_string(payload, &["status"]).is_some_and(|status| canonical(&status) == "idle")
        || event == "notification"
            && find_string(payload, &["notification_type", "notificationType", "kind"]).is_some_and(
                |kind| {
                    let kind = canonical(&kind);
                    kind.contains("idle") || kind.contains("input")
                },
            )
}

fn is_failure(event: &str, payload: &Value) -> bool {
    any(
        event,
        &["error", "failed", "failure", "sessionerror", "turnfailed"],
    ) || payload_success(payload) == Some(false)
}

fn is_stop(agent: AgentKind, event: &str) -> bool {
    any(
        event,
        &[
            "stop",
            "sessionend",
            "sessionended",
            "taskcomplete",
            "taskcompleted",
            "turncomplete",
            "turncompleted",
            "agentend",
            "agentended",
            "afteragent",
            "turnend",
            "threadcomplete",
        ],
    ) || matches!(agent, AgentKind::Codex) && event == "agentturncomplete"
        || matches!(agent, AgentKind::OpenCode) && event == "sessiondeleted"
}

fn is_tool(agent: AgentKind, event: &str, payload: &Value) -> bool {
    any(
        event,
        &[
            "tooluse",
            "pretooluse",
            "posttooluse",
            "posttoolusefailure",
            "beforetool",
            "aftertool",
            "toolpreexecute",
            "toolpostexecute",
            "toolcall",
            "toolresult",
            "toolstart",
            "toolstarted",
            "toolcomplete",
            "toolcompleted",
            "itemstarted",
            "itemcompleted",
            "activity",
            "userpromptsubmit",
            "beforeagent",
            "beforemodel",
            "aftermodel",
            "beforetoolselection",
            "precompress",
            "turnstart",
            "threadstepstart",
            "threadstepdone",
            "context",
        ],
    ) || matches!(agent, AgentKind::Claude) && event == "notification"
        || matches!(agent, AgentKind::OpenCode)
            && matches!(event, "toolexecutebefore" | "toolexecuteafter")
        || matches!(agent, AgentKind::OpenCode)
            && event == "sessionstatus"
            && find_string(payload, &["status"]).is_some_and(|status| canonical(&status) == "busy")
}

fn payload_success(payload: &Value) -> Option<bool> {
    let value = payload.as_object()?.get("success")?;
    value.as_bool().or_else(|| {
        value
            .as_str()
            .and_then(|value| match canonical(value).as_str() {
                "true" | "success" | "ok" | "completed" => Some(true),
                "false" | "failed" | "error" => Some(false),
                _ => None,
            })
    })
}

/// Format of a hook configuration fragment. Plans are descriptive and never
/// write to disk; callers decide whether and how to merge them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookConfigFormat {
    Json,
    Toml,
    JavaScript,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfigSnippet {
    /// Path relative to the agent's own configuration directory, when known.
    pub relative_path: Option<String>,
    pub format: HookConfigFormat,
    pub content: String,
    /// Whether the fragment must be merged rather than replacing the file.
    pub merge_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInstallPlan {
    pub agent: AgentKind,
    /// Command expected to read the native event JSON from stdin.
    pub receiver_command: Vec<String>,
    pub snippets: Vec<HookConfigSnippet>,
    pub notes: Vec<String>,
}

/// Build a side-effect-free installation plan for an agent integration.
///
/// `wmcterm_binary` is kept as one argv item, so paths containing spaces remain
/// safe when the caller serializes the command in the target format.
pub fn hook_install_plan(agent: AgentKind, wmcterm_binary: impl Into<String>) -> HookInstallPlan {
    let binary = wmcterm_binary.into();
    let receiver_command = vec![
        binary.clone(),
        "hook".into(),
        "receive".into(),
        "--agent".into(),
        agent.to_string(),
    ];
    let command = agent_shell_command(agent, &receiver_command);
    let (relative_path, format, content) = match agent {
        AgentKind::Claude => (
            Some("settings.json".into()),
            HookConfigFormat::Json,
            json_hook_fragment(
                &[
                    "SessionStart",
                    "PreToolUse",
                    "PostToolUse",
                    "PostToolUseFailure",
                    "PermissionRequest",
                    "Notification",
                    "SubagentStart",
                    "SubagentStop",
                    "Stop",
                    "SessionEnd",
                ],
                &command,
                true,
                &[
                    "Bash(wmcterm --help)",
                    "Bash(wmcterm --version)",
                    "Bash(wmcterm capabilities)",
                    "Bash(wmcterm status)",
                    "Bash(wmcterm pane list)",
                    "Bash(wmcterm surface list)",
                    "Bash(wmcterm screen read)",
                    "Bash(wmcterm agent-request list)",
                ],
            ),
        ),
        AgentKind::Codex => (
            Some("hooks.json".into()),
            HookConfigFormat::Json,
            json_hook_fragment(
                &[
                    "SessionStart",
                    "UserPromptSubmit",
                    "PermissionRequest",
                    "Interrupt",
                    "SubagentStart",
                    "SubagentStop",
                    "Stop",
                    "SessionEnd",
                ],
                &command,
                true,
                &[],
            ),
        ),
        AgentKind::Gemini => (
            Some("settings.json".into()),
            HookConfigFormat::Json,
            json_hook_fragment(
                &["SessionStart", "BeforeTool", "Notification", "SessionEnd"],
                &command,
                false,
                &[],
            ),
        ),
        AgentKind::OpenCode => (
            Some("plugins/wmcterm.js".into()),
            HookConfigFormat::JavaScript,
            format!("// Forward OpenCode plugin events as JSON on stdin to:\n// {command}"),
        ),
        _ => (
            None,
            HookConfigFormat::Shell,
            format!("# Configure the agent's JSON event hook to pipe stdin to:\n{command}"),
        ),
    };
    HookInstallPlan {
        agent,
        receiver_command,
        snippets: vec![HookConfigSnippet {
            relative_path,
            format,
            content,
            merge_required: true,
        }],
        notes: vec![
            "Preview and merge this fragment; never replace an existing agent configuration file."
                .into(),
            "The receiver command requires the wmcterm hook receive CLI integration.".into(),
            if matches!(agent, AgentKind::Claude) {
                "Claude subagents share the parent process/TUI. Lifecycle hooks can drive a wmcterm pane representation, but cannot relocate Claude's internal subagent TUI."
                    .into()
            } else {
                "Agent-specific lifecycle capabilities depend on the native hook protocol.".into()
            },
        ],
    }
}

fn json_hook_fragment(
    events: &[&str],
    command: &str,
    nested_hooks: bool,
    permission_allow: &[&str],
) -> String {
    let hooks = events.iter().map(|event| {
        let event_command = format!("{command} --event {event}");
        let command_hook = serde_json::json!({ "type": "command", "command": event_command });
        let entries = if nested_hooks {
            serde_json::json!([{ "hooks": [command_hook] }])
        } else {
            serde_json::json!([{ "command": event_command }])
        };
        ((*event).to_owned(), entries)
    });
    let hooks = serde_json::Map::from_iter(hooks);
    let mut fragment = serde_json::json!({ "hooks": hooks });
    if !permission_allow.is_empty() {
        fragment["permissions"]["allow"] = serde_json::json!(permission_allow);
    }
    serde_json::to_string_pretty(&fragment)
        .expect("hook fragment only contains serializable JSON values")
}

fn agent_shell_command(agent: AgentKind, arguments: &[String]) -> String {
    #[cfg(windows)]
    {
        return windows_agent_hook_command(agent, arguments);
    }
    #[cfg(not(windows))]
    {
        let _ = agent;
        arguments
            .iter()
            .map(|argument| shell_argument(argument))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(any(windows, test))]
fn windows_agent_hook_command(agent: AgentKind, arguments: &[String]) -> String {
    let command = windows_hook_command(arguments);
    if matches!(agent, AgentKind::Codex) {
        format!("& {command}")
    } else {
        command
    }
}

#[cfg(any(windows, test))]
fn windows_hook_command(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| {
            if argument
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character))
            {
                argument.clone()
            } else {
                windows_shell_argument(argument)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(not(windows))]
fn shell_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character))
    {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

#[cfg(any(windows, test))]
fn windows_shell_argument(argument: &str) -> String {
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTransition {
    pub state: AgentState,
    pub attention: AttentionState,
    pub notification: Option<String>,
}

pub fn reduce(event: &AgentEvent) -> AgentTransition {
    match event {
        AgentEvent::SessionStart { .. } => AgentTransition {
            state: AgentState::Idle,
            attention: AttentionState::None,
            notification: None,
        },
        AgentEvent::ToolUse { .. } => AgentTransition {
            state: AgentState::Running,
            attention: AttentionState::Activity,
            notification: None,
        },
        AgentEvent::PermissionRequest { message } => AgentTransition {
            state: AgentState::WaitingForPermission,
            attention: AttentionState::NeedsInput,
            notification: Some(message.clone()),
        },
        AgentEvent::NeedsInput { message } => AgentTransition {
            state: AgentState::WaitingForInput,
            attention: AttentionState::NeedsInput,
            notification: Some(message.clone()),
        },
        AgentEvent::TurnComplete { .. } => AgentTransition {
            state: AgentState::Idle,
            attention: AttentionState::None,
            notification: None,
        },
        AgentEvent::Stop {
            success: true,
            message,
        } => AgentTransition {
            state: AgentState::Completed,
            attention: AttentionState::Activity,
            notification: message.clone().or_else(|| Some("Task completed".into())),
        },
        AgentEvent::Stop {
            success: false,
            message,
        } => AgentTransition {
            state: AgentState::Error,
            attention: AttentionState::Error,
            notification: message.clone().or_else(|| Some("Task failed".into())),
        },
    }
}

/// Parse the payload used by OSC 777: `wmcterm;state;message`.
pub fn parse_osc777(payload: &str) -> Option<AgentEvent> {
    let mut parts = payload.splitn(3, ';');
    if parts.next()? != "wmcterm" {
        return None;
    }
    let state = parts.next()?;
    let message = parts.next().unwrap_or_default().to_owned();
    match state {
        "running" => Some(AgentEvent::ToolUse { tool: message }),
        "input" => Some(AgentEvent::NeedsInput { message }),
        "permission" => Some(AgentEvent::PermissionRequest { message }),
        "completed" => Some(AgentEvent::Stop {
            success: true,
            message: Some(message),
        }),
        "error" => Some(AgentEvent::Stop {
            success: false,
            message: Some(message),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    Agent(AgentEvent),
    Notification(String),
    PtyWrite(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct OscScanner {
    payload: Vec<u8>,
    in_osc: bool,
    escape: bool,
    overflow: bool,
    notifications: HashMap<String, PendingNotification>,
}

#[derive(Debug, Default)]
struct PendingNotification {
    title: String,
    body: String,
}

const MAX_OSC_BYTES: usize = 16 * 1024;

impl OscScanner {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if !self.in_osc {
                if self.escape && byte == b']' {
                    self.in_osc = true;
                    self.payload.clear();
                    self.overflow = false;
                    self.escape = false;
                    continue;
                }
                self.escape = byte == 0x1b;
                continue;
            }
            if byte == 0x07 || (self.escape && byte == b'\\') {
                if self.escape && !self.overflow {
                    self.payload.pop();
                }
                let payload = (!self.overflow)
                    .then(|| std::str::from_utf8(&self.payload).ok().map(str::to_owned))
                    .flatten();
                if let Some(payload) = payload {
                    if let Some(value) = payload.strip_prefix("777;") {
                        if let Some(agent) = parse_osc777(value) {
                            events.push(OscEvent::Agent(agent));
                        } else if let Some(notification) = parse_osc777_notification(value) {
                            events.push(OscEvent::Notification(notification));
                        }
                    } else if let Some(value) = payload.strip_prefix("9;") {
                        if !value.trim().is_empty() {
                            events.push(OscEvent::Notification(value.to_owned()));
                        }
                    } else if let Some(value) = payload.strip_prefix("99;") {
                        if let Some(response) = osc99_query_response(value) {
                            events.push(OscEvent::PtyWrite(response));
                        } else if let Some(notification) = self.parse_osc99(value) {
                            events.push(OscEvent::Notification(notification));
                        }
                    }
                }
                self.payload.clear();
                self.in_osc = false;
                self.escape = false;
                continue;
            }
            if self.payload.len() < MAX_OSC_BYTES {
                self.payload.push(byte);
            } else {
                self.overflow = true;
            }
            self.escape = byte == 0x1b;
        }
        events
    }

    fn parse_osc99(&mut self, value: &str) -> Option<String> {
        let Some((metadata, encoded_payload)) = value.split_once(';') else {
            return (!value.trim().is_empty()).then(|| value.to_owned());
        };
        let fields = metadata
            .split(':')
            .filter_map(|field| field.split_once('='))
            .collect::<HashMap<_, _>>();
        let payload = if fields.get("e") == Some(&"1") {
            String::from_utf8(BASE64.decode(encoded_payload).ok()?).ok()?
        } else {
            encoded_payload.to_owned()
        };
        let part = fields.get("p").copied().unwrap_or("title");
        if matches!(part, "close" | "?" | "alive" | "buttons" | "icon") {
            return None;
        }
        let id = fields.get("i").copied().unwrap_or("__anonymous").to_owned();
        if !self.notifications.contains_key(&id) && self.notifications.len() >= 64 {
            return None;
        }
        let pending = self.notifications.entry(id.clone()).or_default();
        match part {
            "title" => append_limited(&mut pending.title, &payload),
            "body" => append_limited(&mut pending.body, &payload),
            _ => return None,
        }
        if fields.get("d") == Some(&"0") {
            return None;
        }
        let pending = self.notifications.remove(&id)?;
        match (pending.title.trim(), pending.body.trim()) {
            ("", "") => None,
            ("", body) => Some(body.to_owned()),
            (title, "") => Some(title.to_owned()),
            (title, body) => Some(format!("{title}: {body}")),
        }
    }
}

fn osc99_query_response(value: &str) -> Option<Vec<u8>> {
    let (metadata, _) = value.split_once(';')?;
    let fields = metadata
        .split(':')
        .filter_map(|field| field.split_once('='))
        .collect::<HashMap<_, _>>();
    if fields.get("p") != Some(&"?") {
        return None;
    }
    let id = fields.get("i").copied().unwrap_or("0");
    Some(format!("\x1b]99;i={id}:p=?;a=focus:p=title,body:o=always\x1b\\").into_bytes())
}

fn append_limited(target: &mut String, value: &str) {
    let remaining = MAX_OSC_BYTES.saturating_sub(target.len());
    let mut end = value.len().min(remaining);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
}

fn parse_osc777_notification(payload: &str) -> Option<String> {
    let mut parts = payload.splitn(3, ';');
    if parts.next()? != "notify" {
        return None;
    }
    let title = parts.next().unwrap_or_default().trim();
    let body = parts.next().unwrap_or_default().trim();
    match (title, body) {
        ("", "") => None,
        ("", body) => Some(body.to_owned()),
        (title, "") => Some(title.to_owned()),
        (title, body) => Some(format!("{title}: {body}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_is_idle_until_activity_begins() {
        let started = reduce(&AgentEvent::SessionStart {
            agent: "codex".into(),
            session_id: Some("session-1".into()),
        });
        assert_eq!(started.state, AgentState::Idle);
        assert_eq!(started.attention, AttentionState::None);

        let active = reduce(&AgentEvent::ToolUse {
            tool: "cargo test".into(),
        });
        assert_eq!(active.state, AgentState::Running);
        assert_eq!(active.attention, AttentionState::Activity);
    }

    #[test]
    fn permission_event_requires_attention() {
        let event = parse_osc777("wmcterm;permission;Approve shell?").unwrap();
        let transition = reduce(&event);
        assert_eq!(transition.state, AgentState::WaitingForPermission);
        assert_eq!(transition.attention, AttentionState::NeedsInput);
    }

    #[test]
    fn scans_chunked_osc_agent_and_notification_events() {
        let mut scanner = OscScanner::default();
        assert!(scanner.feed(b"text\x1b]777;wmcterm;per").is_empty());
        assert!(
            matches!(scanner.feed(b"mission;Approve?\x07").as_slice(), [OscEvent::Agent(AgentEvent::PermissionRequest { message })] if message == "Approve?")
        );
        assert_eq!(
            scanner.feed(b"\x1b]9;Finished\x1b\\"),
            vec![OscEvent::Notification("Finished".into())]
        );
    }

    #[test]
    fn scans_kitty_osc99_and_legacy_osc777_notifications() {
        let mut scanner = OscScanner::default();
        assert_eq!(
            scanner.feed(b"\x1b]99;;Build finished\x1b\\"),
            vec![OscEvent::Notification("Build finished".into())]
        );
        assert!(
            scanner
                .feed(b"\x1b]99;i=agent-1:d=0:p=title;Codex\x1b\\")
                .is_empty()
        );
        assert_eq!(
            scanner.feed(b"\x1b]99;i=agent-1:p=body:e=1;TmVlZHMgaW5wdXQ=\x07"),
            vec![OscEvent::Notification("Codex: Needs input".into())]
        );
        assert_eq!(
            scanner.feed(b"\x1b]777;notify;Agent finished;Review the result\x1b\\"),
            vec![OscEvent::Notification(
                "Agent finished: Review the result".into()
            )]
        );
    }

    #[test]
    fn ignores_oversized_osc_payloads() {
        let mut scanner = OscScanner::default();
        let mut payload = b"\x1b]9;".to_vec();
        payload.extend(std::iter::repeat_n(b'x', MAX_OSC_BYTES + 1));
        payload.push(0x07);
        assert!(scanner.feed(&payload).is_empty());
    }

    #[test]
    fn answers_osc99_capability_queries() {
        let mut scanner = OscScanner::default();
        assert_eq!(
            scanner.feed(b"\x1b]99;i=probe:p=?;\x1b\\"),
            vec![OscEvent::PtyWrite(
                b"\x1b]99;i=probe:p=?;a=focus:p=title,body:o=always\x1b\\".to_vec()
            )]
        );
    }

    #[test]
    fn parses_every_supported_agent_name_and_alias() {
        for kind in AgentKind::ALL {
            assert_eq!(kind.as_str().parse::<AgentKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!("Claude Code".parse(), Ok(AgentKind::Claude));
        assert_eq!("open-code".parse(), Ok(AgentKind::OpenCode));
        assert_eq!("Gemini CLI".parse(), Ok(AgentKind::Gemini));
        assert!("unknown".parse::<AgentKind>().is_err());
    }

    #[test]
    fn detects_agent_processes_without_substring_false_positives() {
        assert_eq!(
            AgentKind::from_process_command("/usr/local/bin/codex --full-auto"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            AgentKind::from_process_command(
                "node /opt/node_modules/@anthropic-ai/claude-code/cli.js"
            ),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::from_process_command("C:\\tools\\gemini.exe"),
            Some(AgentKind::Gemini)
        );
        assert_eq!(AgentKind::from_process_command("compile project"), None);
        assert_eq!(AgentKind::from_process_command("code src/lib.rs"), None);
    }

    #[test]
    fn normalizes_claude_hooks() {
        let start = normalize_hook_json(
            AgentKind::Claude,
            None,
            r#"{"hook_event_name":"SessionStart","session_id":"session-1"}"#,
        )
        .unwrap();
        assert_eq!(
            start,
            AgentEvent::SessionStart {
                agent: "claude".into(),
                session_id: Some("session-1".into())
            }
        );
        assert_eq!(
            normalize_hook_json(
                AgentKind::Claude,
                Some("PreToolUse"),
                r#"{"tool_name":"Bash"}"#,
            )
            .unwrap(),
            AgentEvent::ToolUse {
                tool: "Bash".into()
            }
        );
        assert!(matches!(
            normalize_hook_json(
                AgentKind::Claude,
                Some("PermissionRequest"),
                r#"{"message":"Approve Bash?"}"#,
            )
            .unwrap(),
            AgentEvent::PermissionRequest { message } if message == "Approve Bash?"
        ));
        assert!(matches!(
            normalize_hook_json(AgentKind::Claude, Some("Stop"), "{}").unwrap(),
            AgentEvent::Stop { success: true, .. }
        ));
    }

    #[test]
    fn normalizes_claude_subagent_lifecycle_with_stable_routing_identity() {
        let start_payload = serde_json::json!({
            "session_id": "session-1",
            "transcript_path": "/tmp/root.jsonl",
            "cwd": "/work/project",
            "permission_mode": "manual",
            "hook_event_name": "SubagentStart",
            "agent_id": "a63e680",
            "agent_type": "Explore"
        });
        let start = normalize_claude_hook_event(None, &start_payload).unwrap();
        assert_eq!(start.kind, ClaudeHookKind::SubagentStart);
        assert_eq!(start.agent_id.as_deref(), Some("a63e680"));
        assert_eq!(start.agent_type.as_deref(), Some("Explore"));
        assert_eq!(start.cwd.as_deref(), Some("/work/project"));
        assert_eq!(start.route_key(), "claude:session-1:a63e680");
        assert_eq!(start.pane_action(), ClaudePaneAction::OpenSubagentStatus);
        assert!(matches!(
            start.to_agent_event(),
            AgentEvent::ToolUse { tool } if tool == "subagent started: Explore"
        ));

        let stop_payload = serde_json::json!({
            "session_id": "session-1",
            "hook_event_name": "SubagentStop",
            "agent_id": "a63e680",
            "agent_type": "Explore",
            "agent_transcript_path": "/tmp/agent-a63e680.jsonl",
            "last_assistant_message": "Inspection complete"
        });
        let stop = normalize_claude_hook_event(None, &stop_payload).unwrap();
        assert_eq!(stop.kind, ClaudeHookKind::SubagentStop);
        assert_eq!(
            stop.agent_transcript_path.as_deref(),
            Some("/tmp/agent-a63e680.jsonl")
        );
        assert_eq!(stop.message.as_deref(), Some("Inspection complete"));
        assert_eq!(stop.pane_action(), ClaudePaneAction::CloseSubagentStatus);
        // A subagent stop must not complete the whole terminal agent state.
        assert!(matches!(stop.to_agent_event(), AgentEvent::ToolUse { .. }));
        assert!(matches!(
            normalize_hook_event(AgentKind::Claude, None, &stop_payload).unwrap(),
            AgentEvent::ToolUse { .. }
        ));
    }

    #[test]
    fn normalizes_claude_tool_notification_and_session_end_payloads() {
        let tool = normalize_claude_hook_event(
            None,
            &serde_json::json!({
                "session_id": "session-2",
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_use_id": "toolu_42",
                "tool_input": {"command": "cargo test"},
                "tool_response": {"stdout": "ok"}
            }),
        )
        .unwrap();
        assert_eq!(tool.kind, ClaudeHookKind::PostToolUse);
        assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
        assert_eq!(tool.tool_use_id.as_deref(), Some("toolu_42"));

        let notification = normalize_claude_hook_event(
            Some("Notification"),
            &serde_json::json!({
                "session_id": "session-2",
                "notification_type": "idle_prompt",
                "message": "Claude is waiting"
            }),
        )
        .unwrap();
        assert!(matches!(
            notification.to_agent_event(),
            AgentEvent::NeedsInput { message } if message == "Claude is waiting"
        ));

        let end = normalize_claude_hook_event(
            None,
            &serde_json::json!({
                "session_id": "session-2",
                "hook_event_name": "SessionEnd",
                "reason": "prompt_input_exit"
            }),
        )
        .unwrap();
        assert_eq!(end.kind, ClaudeHookKind::SessionEnd);
        assert!(matches!(
            end.to_agent_event(),
            AgentEvent::Stop { success: true, message: Some(message) }
                if message == "prompt_input_exit"
        ));
    }

    #[test]
    fn claude_stop_is_a_turn_boundary_and_preserves_goal_continuation() {
        let stopped = normalize_claude_hook_event(
            None,
            &serde_json::json!({
                "session_id": "goal-session",
                "hook_event_name": "Stop",
                "stop_hook_active": false,
                "last_assistant_message": "Checking the goal"
            }),
        )
        .unwrap();
        assert_eq!(stopped.pane_action(), ClaudePaneAction::UpdateAgentStatus);
        assert!(matches!(
            stopped.to_agent_event(),
            AgentEvent::TurnComplete {
                continuing: false,
                ..
            }
        ));
        assert_eq!(reduce(&stopped.to_agent_event()).state, AgentState::Idle);

        let continuing = normalize_claude_hook_event(
            None,
            &serde_json::json!({
                "session_id": "goal-session",
                "hook_event_name": "Stop",
                "stop_hook_active": true,
                "last_assistant_message": "Continuing toward the goal"
            }),
        )
        .unwrap();
        assert!(continuing.stop_hook_active);
        assert!(matches!(
            continuing.to_agent_event(),
            AgentEvent::TurnComplete {
                continuing: true,
                ..
            }
        ));
        // `stop_hook_active` records that a previous Stop hook continued the
        // turn; it does not reveal whether this Stop will be blocked again.
        assert_eq!(reduce(&continuing.to_agent_event()).state, AgentState::Idle);
    }

    #[test]
    fn claude_structured_events_fail_closed_without_top_level_route_identity() {
        let nested_spoof = serde_json::json!({
            "hook_event_name": "SubagentStart",
            "payload": {"session_id": "nested", "agent_id": "nested-agent"}
        });
        assert!(matches!(
            normalize_claude_hook_event(None, &nested_spoof),
            Err(NormalizeHookError::MissingIdentity {
                field: "session_id",
                ..
            })
        ));
        assert!(matches!(
            normalize_claude_hook_event(
                Some("SubagentStop"),
                &serde_json::json!({"session_id": "session-3"})
            ),
            Err(NormalizeHookError::MissingIdentity {
                field: "agent_id",
                ..
            })
        ));
    }

    #[test]
    fn normalizes_codex_and_opencode_native_events() {
        assert_eq!(
            normalize_hook_json(
                AgentKind::Codex,
                None,
                r#"{"type":"item-started","item":{"item_type":"command_execution"}}"#,
            )
            .unwrap(),
            AgentEvent::ToolUse {
                tool: "command_execution".into()
            }
        );
        assert!(matches!(
            normalize_hook_json(
                AgentKind::Codex,
                None,
                r#"{"type":"agent-turn-complete","message":"done"}"#,
            )
            .unwrap(),
            AgentEvent::Stop { success: true, message: Some(message) } if message == "done"
        ));
        let interrupted = normalize_hook_json(
            AgentKind::Codex,
            None,
            r#"{"hook_event_name":"Interrupt","session_id":"session-1","turn_id":"turn-2","cwd":"/work/project","model":"gpt-5.3-codex","permission_mode":"on-request","transcript_path":"/tmp/codex.jsonl"}"#,
        )
        .unwrap();
        assert!(matches!(
            interrupted,
            AgentEvent::TurnComplete {
                continuing: false,
                message: None
            }
        ));
        assert_eq!(reduce(&interrupted).state, AgentState::Idle);
        assert!(matches!(
            normalize_hook_json(
                AgentKind::OpenCode,
                None,
                r#"{"event":"permission.updated","data":{"message":"Allow edit?"}}"#,
            )
            .unwrap(),
            AgentEvent::PermissionRequest { message } if message == "Allow edit?"
        ));
        assert!(matches!(
            normalize_hook_json(
                AgentKind::OpenCode,
                Some("session.idle"),
                r#"{"payload":{"message":"Your response is needed"}}"#,
            )
            .unwrap(),
            AgentEvent::NeedsInput { .. }
        ));
        assert!(matches!(
            normalize_hook_json(
                AgentKind::OpenCode,
                None,
                r#"{"type":"session.status","data":{"status":"busy"}}"#,
            )
            .unwrap(),
            AgentEvent::ToolUse { .. }
        ));
    }

    #[test]
    fn normalizes_codex_subagent_hooks_with_stable_routing_identity() {
        let start = normalize_codex_subagent_hook_event(
            None,
            &serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-7",
                "hook_event_name": "SubagentStart",
                "agent_id": "agent-42",
                "agent_type": "explorer",
                "cwd": "/work/project",
                "model": "gpt-5.3-codex",
                "permission_mode": "default",
                "transcript_path": "/tmp/parent.jsonl"
            }),
        )
        .unwrap();
        assert_eq!(start.kind, CodexSubagentHookKind::Start);
        assert_eq!(start.session_id, "session-1");
        assert_eq!(start.turn_id, "turn-7");
        assert_eq!(start.agent_id, "agent-42");
        assert_eq!(start.agent_type, "explorer");
        assert_eq!(start.route_key(), ("session-1", "agent-42"));
        assert!(matches!(
            start.to_agent_event(),
            AgentEvent::ToolUse { tool } if tool == "Codex subagent started: explorer"
        ));

        let stop = normalize_codex_subagent_hook_event(
            Some("SubagentStop"),
            &serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-7",
                "hook_event_name": "SubagentStop",
                "agent_id": "agent-42",
                "agent_type": "explorer",
                "agent_transcript_path": "/tmp/agent-42.jsonl",
                "last_assistant_message": "Inspection complete"
            }),
        )
        .unwrap();
        assert_eq!(stop.kind, CodexSubagentHookKind::Stop);
        assert_eq!(
            stop.agent_transcript_path.as_deref(),
            Some("/tmp/agent-42.jsonl")
        );
        assert_eq!(
            stop.last_assistant_message.as_deref(),
            Some("Inspection complete")
        );
        assert!(matches!(stop.to_agent_event(), AgentEvent::ToolUse { .. }));
        assert_eq!(reduce(&stop.to_agent_event()).state, AgentState::Running);
    }

    #[test]
    fn codex_subagent_hooks_require_top_level_0151_identity_fields() {
        let nested = serde_json::json!({
            "hook_event_name": "SubagentStart",
            "payload": {
                "session_id": "nested",
                "turn_id": "nested-turn",
                "agent_id": "nested-agent",
                "agent_type": "worker"
            }
        });
        assert!(matches!(
            normalize_codex_subagent_hook_event(None, &nested),
            Err(NormalizeHookError::MissingIdentity {
                field: "session_id",
                ..
            })
        ));
        for field in ["turn_id", "agent_id", "agent_type"] {
            let mut payload = serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-7",
                "hook_event_name": "SubagentStop",
                "agent_id": "agent-42",
                "agent_type": "explorer"
            });
            payload.as_object_mut().unwrap().remove(field);
            assert!(matches!(
                normalize_codex_subagent_hook_event(None, &payload),
                Err(NormalizeHookError::MissingIdentity {
                    field: missing,
                    ..
                }) if missing == field
            ));
        }
    }

    #[test]
    fn codex_subagent_hook_rejects_explicit_and_payload_event_mismatches() {
        let payload = |hook_event_name: &str| {
            serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-7",
                "hook_event_name": hook_event_name,
                "agent_id": "agent-42",
                "agent_type": "explorer"
            })
        };
        assert!(matches!(
            normalize_codex_subagent_hook_event(
                Some("SubagentStart"),
                &payload("SubagentStop")
            ),
            Err(NormalizeHookError::EventNameMismatch {
                argument,
                payload,
                ..
            }) if argument == "SubagentStart" && payload == "SubagentStop"
        ));
        assert!(matches!(
            normalize_codex_subagent_hook_event(
                Some("SubagentStop"),
                &payload("SubagentStart")
            ),
            Err(NormalizeHookError::EventNameMismatch {
                argument,
                payload,
                ..
            }) if argument == "SubagentStop" && payload == "SubagentStart"
        ));
    }

    #[test]
    fn codex_subagent_structured_validation_rejects_blank_routing_identity() {
        let valid = CodexSubagentHookEvent {
            kind: CodexSubagentHookKind::Start,
            session_id: "session-1".into(),
            turn_id: "turn-7".into(),
            agent_id: "agent-42".into(),
            agent_type: "explorer".into(),
            agent_transcript_path: None,
            last_assistant_message: None,
        };
        for field in ["session_id", "turn_id", "agent_id", "agent_type"] {
            let mut event = valid.clone();
            match field {
                "session_id" => event.session_id = " \t ".into(),
                "turn_id" => event.turn_id = " \t ".into(),
                "agent_id" => event.agent_id = " \t ".into(),
                "agent_type" => event.agent_type = " \t ".into(),
                _ => unreachable!(),
            }
            assert!(matches!(
                event.validate(),
                Err(NormalizeHookError::MissingIdentity {
                    field: missing,
                    ..
                }) if missing == field
            ));
        }
    }

    #[test]
    fn normalizes_gemini_notifications_and_generic_agent_events() {
        assert!(matches!(
            normalize_hook_json(
                AgentKind::Gemini,
                Some("Notification"),
                r#"{"notification_type":"permission_prompt","prompt":"Run command?"}"#,
            )
            .unwrap(),
            AgentEvent::PermissionRequest { message } if message == "Run command?"
        ));
        for agent in [
            AgentKind::Grok,
            AgentKind::Pi,
            AgentKind::Omp,
            AgentKind::Amp,
        ] {
            assert!(matches!(
                normalize_hook_json(agent, Some("tool_call"), r#"{"tool":"read_file"}"#)
                    .unwrap(),
                AgentEvent::ToolUse { tool } if tool == "read_file"
            ));
            assert!(matches!(
                normalize_hook_json(
                    agent,
                    Some("turn_failed"),
                    r#"{"error":"network unavailable"}"#,
                )
                .unwrap(),
                AgentEvent::Stop { success: false, message: Some(message) }
                    if message == "network unavailable"
            ));
        }
    }

    #[test]
    fn explicit_event_wins_and_errors_are_descriptive() {
        assert!(matches!(
            normalize_hook_json(
                AgentKind::Amp,
                Some("needs_input"),
                r#"{"event":"tool_call","message":"Choose one"}"#,
            )
            .unwrap(),
            AgentEvent::NeedsInput { message } if message == "Choose one"
        ));
        assert!(matches!(
            normalize_hook_json(AgentKind::Pi, None, "{}"),
            Err(HookJsonError::Normalize(
                NormalizeHookError::MissingEventName
            ))
        ));
        assert!(matches!(
            normalize_hook_json(AgentKind::Pi, Some("mystery"), "{}"),
            Err(HookJsonError::Normalize(
                NormalizeHookError::UnsupportedEvent { .. }
            ))
        ));
        assert!(matches!(
            normalize_hook_json(AgentKind::Pi, Some("tool_call"), "[]"),
            Err(HookJsonError::Normalize(NormalizeHookError::InvalidPayload))
        ));
        assert!(matches!(
            normalize_hook_json(AgentKind::Pi, None, "not-json"),
            Err(HookJsonError::Json(_))
        ));
    }

    #[test]
    fn event_envelope_extracts_nested_hook_identity_and_flattens_for_legacy_rpc() {
        let payload = serde_json::json!({
            "terminal_lifecycle_id": "life-42",
            "data": {
                "agent_pid": "1234",
                "pid_start_time": 987654_u64,
                "sequence": "7"
            }
        });
        let envelope = AgentEventEnvelope::from_hook_payload(
            AgentEvent::NeedsInput {
                message: "Review changes".into(),
            },
            &payload,
            Some("surface-1".into()),
        );
        assert_eq!(envelope.lifecycle_id.as_deref(), Some("life-42"));
        assert_eq!(envelope.agent_pid, Some(1234));
        assert_eq!(envelope.pid_start_time, Some(987654));
        assert_eq!(envelope.sequence, Some(7));
        assert_eq!(envelope.version, Some(AgentEventEnvelope::CURRENT_VERSION));

        let params = envelope.to_legacy_rpc_params().unwrap();
        assert_eq!(params["event"], "needs_input");
        assert_eq!(params["surface"], "surface-1");
        assert_eq!(params["surface_id"], "surface-1");
        assert_eq!(params["lifecycle_id"], "life-42");
        assert_eq!(params["agent_pid"], 1234);
        assert_eq!(params["sequence"], 7);
        assert_eq!(
            serde_json::from_value::<AgentEvent>(params).unwrap(),
            envelope.event
        );
    }

    #[test]
    fn compatible_envelope_parser_preserves_legacy_agent_events() {
        let legacy = serde_json::json!({
            "event": "tool_use",
            "tool": "cargo test"
        });
        let parsed = AgentEventEnvelope::from_compatible_value(legacy).unwrap();
        assert_eq!(
            parsed.event,
            AgentEvent::ToolUse {
                tool: "cargo test".into()
            }
        );
        assert_eq!(parsed.version, Some(AgentEventEnvelope::CURRENT_VERSION));

        let canonical = serde_json::to_value(AgentEventEnvelope {
            surface_id: Some("surface-2".into()),
            lifecycle_id: Some("life-2".into()),
            agent_pid: Some(55),
            pid_start_time: Some(66),
            version: Some(2),
            sequence: Some(9),
            event: AgentEvent::Stop {
                success: true,
                message: None,
            },
        })
        .unwrap();
        let parsed = AgentEventEnvelope::from_compatible_value(canonical).unwrap();
        assert_eq!(parsed.surface_id.as_deref(), Some("surface-2"));
        assert_eq!(parsed.sequence, Some(9));
        assert!(matches!(
            parsed.event,
            AgentEvent::Stop { success: true, .. }
        ));
    }

    #[test]
    fn process_identity_comparison_rejects_pid_reuse_and_binding_mismatches() {
        let expected = AgentProcessIdentity {
            pid: 42,
            pid_start_time: 100,
            surface_id: "surface-a".into(),
            lifecycle_id: "life-a".into(),
            controlling_tty: Some(7),
        };
        assert_eq!(compare_agent_process_identity(&expected, &expected), Ok(()));

        let mut observed = expected.clone();
        observed.pid_start_time = 101;
        assert_eq!(
            compare_agent_process_identity(&expected, &observed),
            Err(ProcessIdentityError::PidReused { pid: 42 })
        );
        observed = expected.clone();
        observed.surface_id = "surface-b".into();
        assert!(matches!(
            compare_agent_process_identity(&expected, &observed),
            Err(ProcessIdentityError::IdentityMismatch {
                field: "surface id",
                ..
            })
        ));
        observed = expected.clone();
        observed.lifecycle_id = "life-b".into();
        assert!(matches!(
            compare_agent_process_identity(&expected, &observed),
            Err(ProcessIdentityError::IdentityMismatch {
                field: "lifecycle id",
                ..
            })
        ));
        observed = expected.clone();
        observed.controlling_tty = Some(8);
        assert!(matches!(
            compare_agent_process_identity(&expected, &observed),
            Err(ProcessIdentityError::IdentityMismatch {
                field: "controlling tty",
                ..
            })
        ));

        let mut tty_optional = expected;
        tty_optional.controlling_tty = None;
        assert_eq!(
            compare_agent_process_identity(&tty_optional, &observed),
            Ok(())
        );
    }

    #[test]
    fn install_plans_are_side_effect_free_and_cover_every_agent() {
        for agent in AgentKind::ALL {
            let plan = hook_install_plan(agent, "/opt/wmc term/wmcterm");
            assert_eq!(plan.agent, agent);
            assert_eq!(plan.receiver_command[0], "/opt/wmc term/wmcterm");
            assert_eq!(plan.receiver_command.last().unwrap(), agent.as_str());
            assert_eq!(plan.snippets.len(), 1);
            assert!(plan.snippets[0].merge_required);
            assert!(plan.snippets[0].content.contains("wmcterm"));
        }
        let claude = hook_install_plan(AgentKind::Claude, "wmcterm");
        assert_eq!(
            claude.snippets[0].relative_path.as_deref(),
            Some("settings.json")
        );
        serde_json::from_str::<Value>(&claude.snippets[0].content).unwrap();
        let claude_json: Value = serde_json::from_str(&claude.snippets[0].content).unwrap();
        for event in [
            "SessionStart",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionRequest",
            "Notification",
            "SubagentStart",
            "SubagentStop",
            "Stop",
            "SessionEnd",
        ] {
            assert!(claude_json["hooks"][event].is_array(), "missing {event}");
        }
        for safe_command in [
            "Bash(wmcterm capabilities)",
            "Bash(wmcterm status)",
            "Bash(wmcterm pane list)",
            "Bash(wmcterm surface list)",
            "Bash(wmcterm screen read)",
            "Bash(wmcterm agent-request list)",
        ] {
            assert!(
                claude_json["permissions"]["allow"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::String(safe_command.into()))
            );
        }
        let quoted = hook_install_plan(AgentKind::Gemini, "/opt/wmc \"term\"/wmc'term");
        serde_json::from_str::<Value>(&quoted.snippets[0].content).unwrap();
        let codex = hook_install_plan(AgentKind::Codex, "wmcterm");
        assert_eq!(codex.snippets[0].format, HookConfigFormat::Json);
        assert_eq!(
            codex.snippets[0].relative_path.as_deref(),
            Some("hooks.json")
        );
        let codex_json: Value = serde_json::from_str(&codex.snippets[0].content).unwrap();
        assert!(codex_json["hooks"]["PermissionRequest"].is_array());
        assert!(codex_json["hooks"]["Interrupt"].is_array());
        assert!(codex_json["hooks"]["SubagentStart"].is_array());
        assert!(codex_json["hooks"]["SubagentStop"].is_array());
        assert!(codex_json["hooks"]["Stop"].is_array());
    }

    #[test]
    fn windows_hook_command_quotes_installed_paths_for_create_process() {
        assert_eq!(
            windows_shell_argument(r#"C:\Program Files\wmcterm\wmcterm.exe"#),
            r#""C:\Program Files\wmcterm\wmcterm.exe""#
        );
        assert_eq!(
            windows_shell_argument(r#"C:\tools\wmc"term\"#),
            r#""C:\tools\wmc\"term\\""#
        );
    }

    #[test]
    fn windows_hook_command_is_bash_compatible_and_does_not_quote_event_as_a_command() {
        let arguments = [
            r"C:\Program Files\wmcterm\wmcterm.exe".to_owned(),
            "hook".to_owned(),
            "receive".to_owned(),
            "--event".to_owned(),
            "SessionStart".to_owned(),
        ];
        assert_eq!(
            windows_hook_command(&arguments),
            r#""C:\Program Files\wmcterm\wmcterm.exe" hook receive --event SessionStart"#
        );
        assert_eq!(
            windows_agent_hook_command(AgentKind::Codex, &arguments),
            r#"& "C:\Program Files\wmcterm\wmcterm.exe" hook receive --event SessionStart"#
        );
        assert_eq!(
            windows_agent_hook_command(AgentKind::Claude, &arguments),
            r#""C:\Program Files\wmcterm\wmcterm.exe" hook receive --event SessionStart"#
        );
    }
}
