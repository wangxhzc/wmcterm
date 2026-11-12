use super::*;

impl WmctermApplication {
    pub(super) fn session_snapshot(&self, include_history: bool) -> Session {
        let mut histories: HashMap<_, _> = if include_history {
            self.terminals
                .iter()
                .filter_map(|(&id, runtime)| {
                    runtime
                        .session
                        .request_history()
                        .map(|receiver| (id, receiver))
                })
                .collect()
        } else {
            HashMap::new()
        };
        let history_deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let terminals = self
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
            .map(|terminal_id| {
                let resume = self
                    .managed_codex
                    .get(&terminal_id)
                    .filter(|codex| !codex.saved.session_id.is_empty())
                    .map(|codex| PersistedAgentResume {
                        agent: AgentResumeInfo::Codex {
                            session_id: codex.saved.session_id.clone(),
                        },
                        managed: true,
                    })
                    .or_else(|| {
                        resume_info(
                            &self.state,
                            &self.agent_sessions,
                            &self.foreground_agents,
                            terminal_id,
                        )
                    });
                TerminalResume {
                    managed_codex: self
                        .managed_codex
                        .get(&terminal_id)
                        .map(|codex| codex.saved.clone()),
                    terminal_id,
                    cwd: self
                        .managed_codex
                        .get(&terminal_id)
                        .and_then(|codex| codex.cwd.clone())
                        .or_else(|| self.cwd_for_terminal(terminal_id))
                        .unwrap_or_else(|| env::current_dir().unwrap_or_default()),
                    command: match resume.as_ref() {
                        Some(resume) if resume.managed => None,
                        Some(resume) => Some(manual_agent_resume_command(&resume.agent.command())),
                        None => self
                            .commands
                            .get(&terminal_id)
                            .filter(|command| {
                                !is_manual_agent_resume_command(command)
                                    || can_preserve_manual_resume(
                                        &self.state,
                                        &self.agent_sessions,
                                        terminal_id,
                                    )
                            })
                            .cloned(),
                    },
                    agent: resume.map(|resume| resume.agent),
                    scrollback: histories
                        .remove(&terminal_id)
                        .and_then(|receiver| {
                            receiver
                                .recv_timeout(
                                    history_deadline
                                        .saturating_duration_since(std::time::Instant::now()),
                                )
                                .ok()
                        })
                        .or_else(|| {
                            self.session_persistence
                                .as_ref()
                                .and_then(|persistence| persistence.history(terminal_id))
                        })
                        .or_else(|| self.restored_history.get(&terminal_id).cloned()),
                }
            })
            .collect();
        Session {
            version: SESSION_VERSION,
            app: self.state.clone(),
            terminals,
            agent_sessions: self.agent_sessions.values().cloned().collect(),
            window: Some(WindowState {
                width: self.window_size.width.max(600),
                height: self.window_size.height.max(360),
            }),
        }
    }
}

struct SaveJob {
    session: Session,
    completed: Option<mpsc::SyncSender<Result<(), String>>>,
}

pub(super) struct SessionPersistence {
    requests: Option<flume::Sender<SaveJob>>,
    worker: Option<thread::JoinHandle<()>>,
    histories: HashMap<TerminalId, String>,
    history_requests: HashMap<TerminalId, mpsc::Receiver<String>>,
    last_history: std::time::Instant,
    last_poll: std::time::Instant,
    saved: Vec<u8>,
    history_revision: u64,
    failures: flume::Receiver<()>,
}

impl SessionPersistence {
    pub(super) fn new(path: PathBuf, histories: HashMap<TerminalId, String>) -> Result<Self> {
        let (requests, incoming) = flume::bounded::<SaveJob>(1);
        let (failed, failures) = flume::bounded(1);
        let worker = thread::Builder::new()
            .name("wmcterm-session-save".into())
            .spawn(move || {
                while let Ok(mut job) = incoming.recv() {
                    discard_unavailable_codex_resumes(&mut job.session);
                    let result =
                        save_session(&path, &job.session).map_err(|error| error.to_string());
                    if let Err(error) = &result {
                        tracing::warn!(%error, "saving session failed");
                        let _ = failed.try_send(());
                    }
                    if let Some(completed) = job.completed {
                        let _ = completed.send(result);
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            worker: Some(worker),
            histories,
            history_requests: HashMap::new(),
            last_history: std::time::Instant::now(),
            last_poll: std::time::Instant::now(),
            saved: Vec::new(),
            history_revision: 0,
            failures,
        })
    }

    pub(super) fn history(&self, id: TerminalId) -> Option<String> {
        self.histories.get(&id).cloned()
    }

    pub(super) fn tick(&mut self, app: &WmctermApplication) {
        if self.failures.try_recv().is_ok() {
            self.saved.clear();
        }
        self.history_requests
            .retain(|id, receiver| match receiver.try_recv() {
                Ok(history) => {
                    self.histories.insert(*id, history);
                    self.history_revision = self.history_revision.wrapping_add(1);
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => false,
            });
        self.histories.retain(|id, _| {
            app.state
                .surfaces
                .values()
                .any(|surface| surface.kind == SurfaceKind::Terminal(*id))
        });
        if self.last_history.elapsed() >= std::time::Duration::from_secs(60) {
            self.last_history = std::time::Instant::now();
            for (&id, runtime) in &app.terminals {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.history_requests.entry(id)
                    && let Some(receiver) = runtime.session.request_history()
                {
                    entry.insert(receiver);
                }
            }
        }
        if self.last_poll.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_poll = std::time::Instant::now();
        let mut session = app.session_snapshot(false);
        let histories: HashMap<_, _> = session
            .terminals
            .iter_mut()
            .filter_map(|terminal| {
                terminal
                    .scrollback
                    .take()
                    .map(|history| (terminal.terminal_id, history))
            })
            .collect();
        let Ok(fingerprint) = serde_json::to_vec(&(&session, self.history_revision)) else {
            return;
        };
        if fingerprint == self.saved {
            return;
        }
        for terminal in &mut session.terminals {
            terminal.scrollback = self
                .history(terminal.terminal_id)
                .or_else(|| histories.get(&terminal.terminal_id).cloned());
        }
        if self.requests.as_ref().is_some_and(|requests| {
            requests
                .try_send(SaveJob {
                    session,
                    completed: None,
                })
                .is_ok()
        }) {
            self.saved = fingerprint;
        }
    }

    pub(super) fn finish(&mut self, session: Session) -> Result<()> {
        let (completed, result) = mpsc::sync_channel(1);
        self.requests
            .as_ref()
            .context("session writer is closed")?
            .send(SaveJob {
                session,
                completed: Some(completed),
            })?;
        result.recv()?.map_err(anyhow::Error::msg)
    }
}

impl Drop for SessionPersistence {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> Session {
        Session {
            version: SESSION_VERSION,
            app: AppState::new(name, env::temp_dir()),
            terminals: Vec::new(),
            agent_sessions: Vec::new(),
            window: None,
        }
    }

    #[test]
    fn final_save_follows_queued_checkpoint_and_replaces_existing_file() {
        let path =
            env::temp_dir().join(format!("wmcterm-checkpoint-{}.json", uuid::Uuid::new_v4()));
        let mut writer = SessionPersistence::new(path.clone(), HashMap::new()).unwrap();
        writer
            .requests
            .as_ref()
            .unwrap()
            .send(SaveJob {
                session: session("checkpoint"),
                completed: None,
            })
            .unwrap();
        writer.finish(session("final")).unwrap();
        drop(writer);
        assert_eq!(load_session(&path).unwrap().app.workspaces[0].name, "final");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn final_save_reports_disk_errors() {
        let path = env::temp_dir().join(format!("wmcterm-invalid-save-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let mut writer = SessionPersistence::new(path.clone(), HashMap::new()).unwrap();
        assert!(
            writer
                .finish(session("cannot replace a directory"))
                .is_err()
        );
        drop(writer);
        std::fs::remove_dir(path).unwrap();
    }
}
