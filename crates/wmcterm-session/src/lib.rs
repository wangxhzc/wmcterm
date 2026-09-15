use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wmcterm_core::{AgentRunMode, AgentSession, AppState, SurfaceId, TerminalId};

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub app: AppState,
    pub terminals: Vec<TerminalResume>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_sessions: Vec<AgentSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResume {
    pub terminal_id: TerminalId,
    pub cwd: PathBuf,
    pub command: Option<String>,
    pub agent: Option<AgentResumeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_codex: Option<ManagedCodexResume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCodexResume {
    pub session_id: String,
    #[serde(default)]
    pub mode: AgentRunMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_surface: Option<SurfaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "agent", rename_all = "snake_case")]
pub enum AgentResumeInfo {
    Claude { session_id: String },
    Codex { session_id: String },
    OpenCode { session_id: String },
}

impl AgentResumeInfo {
    pub fn command(&self) -> String {
        match self {
            Self::Claude { session_id } => format!("claude --resume {session_id}"),
            Self::Codex { session_id } => format!("codex resume {session_id}"),
            Self::OpenCode { session_id } => format!("opencode --session {session_id}"),
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid session: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(u32),
}

pub fn state_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "wmcterm", "wmcterm").map(|dirs| dirs.data_dir().join("state.json"))
}

pub fn save(path: &Path, session: &Session) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut file, session)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Session, SessionError> {
    let session: Session = serde_json::from_reader(fs::File::open(path)?)?;
    if session.version != SESSION_VERSION {
        return Err(SessionError::UnsupportedVersion(session.version));
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn managed_codex_restore_round_trips_modes_and_keeps_legacy_defaults() {
        let legacy = serde_json::json!({
            "terminal_id": TerminalId::new_v4(), "cwd": "/tmp", "command": null,
            "agent": {"agent": "codex", "session_id": "old-thread"}
        });
        let restored: TerminalResume = serde_json::from_value(legacy).unwrap();
        assert!(restored.managed_codex.is_none());
        for mode in [
            AgentRunMode::Interactive,
            AgentRunMode::Unattended,
            AgentRunMode::FullAccess,
        ] {
            let saved = ManagedCodexResume {
                session_id: "thread".into(),
                mode,
                parent_surface: Some(SurfaceId::new_v4()),
                permissions: None,
            };
            let decoded: ManagedCodexResume =
                serde_json::from_value(serde_json::to_value(&saved).unwrap()).unwrap();
            assert_eq!(decoded, saved);
        }
        let old: ManagedCodexResume =
            serde_json::from_value(serde_json::json!({"session_id": "thread"})).unwrap();
        assert_eq!(old.mode, AgentRunMode::Interactive);
    }

    #[test]
    fn atomically_round_trips_a_session() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wmcterm-session-{nonce}.json"));
        let surface_id = wmcterm_core::SurfaceId::new_v4();
        let mut agent_session = AgentSession::new(surface_id);
        agent_session
            .apply_event(&wmcterm_core::AgentSessionEvent {
                event_id: "life-1:start".into(),
                lifecycle_generation: 1,
                lifecycle_id: Some("life-1".into()),
                sequence: Some(1),
                occurred_at_unix_ms: 100,
                kind: wmcterm_core::AgentSessionEventKind::Lifecycle(
                    wmcterm_core::AgentLifecycleEvent::Started {
                        launch: wmcterm_core::AgentLaunchInfo {
                            agent_kind: wmcterm_core::AgentKindId::new("codex"),
                            session_id: Some("session-1".into()),
                            pid: Some(1234),
                            process_started_at_unix_ms: Some(99),
                            cwd: Some(PathBuf::from("/tmp/project")),
                            transcript: None,
                            launch_command: Some("codex".into()),
                        },
                    },
                ),
            })
            .unwrap();
        agent_session
            .apply_event(&wmcterm_core::AgentSessionEvent {
                event_id: "life-1:permission-2".into(),
                lifecycle_generation: 1,
                lifecycle_id: Some("life-1".into()),
                sequence: Some(2),
                occurred_at_unix_ms: 101,
                kind: wmcterm_core::AgentSessionEventKind::ActionRequired(
                    wmcterm_core::AgentActionRequest {
                        request_id: "permission:2".into(),
                        kind: wmcterm_core::AgentRequestKind::Permission,
                        message: "Allow command?".into(),
                        choices: vec!["allow".into(), "deny".into()],
                    },
                ),
            })
            .unwrap();
        let expected_agent_session = agent_session.clone();
        let expected = Session {
            version: SESSION_VERSION,
            app: AppState::default(),
            terminals: vec![TerminalResume {
                managed_codex: None,
                terminal_id: TerminalId::new_v4(),
                cwd: PathBuf::from("/tmp/project"),
                command: Some("cargo test".into()),
                agent: Some(AgentResumeInfo::Codex {
                    session_id: "session-1".into(),
                }),
                scrollback: Some("previous output\n".into()),
            }],
            agent_sessions: vec![agent_session],
            window: Some(WindowState {
                width: 1280,
                height: 800,
            }),
        };
        save(&path, &expected).unwrap();
        let actual = load(&path).unwrap();
        assert_eq!(actual.version, SESSION_VERSION);
        assert_eq!(actual.app.workspaces.len(), 1);
        assert_eq!(
            actual.terminals[0].scrollback.as_deref(),
            Some("previous output\n")
        );
        assert_eq!(actual.window, expected.window);
        assert_eq!(actual.agent_sessions, vec![expected_agent_session]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loads_sessions_without_window_geometry() {
        let session: Session = serde_json::from_str(
            r#"{"version":1,"app":{"workspaces":[],"panes":{},"surfaces":{},"notifications":[],"active_workspace":0},"terminals":[]}"#,
        )
        .unwrap();
        assert_eq!(session.window, None);
    }
}
