use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MAX_COMMAND_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn send_pty_input(pty: &PtySession, events: &TerminalEventSender, bytes: Vec<u8>) {
    if pty.send(PtyCommand::Input(bytes)).is_err() {
        let _ = events.send(SessionEvent::InputRejected);
    }
}

#[derive(Clone, Default)]
pub(super) struct RuntimeControl {
    stopped: Arc<AtomicBool>,
    bytes: Arc<AtomicUsize>,
    rejected: Arc<AtomicBool>,
}

impl RuntimeControl {
    fn weight(command: &SessionCommand) -> usize {
        match command {
            SessionCommand::Input(bytes) => bytes.len(),
            SessionCommand::Paste(text)
            | SessionCommand::PasteLine(text)
            | SessionCommand::RestoreHistory(text) => text.len(),
            SessionCommand::Search { query, .. } => query.len(),
            SessionCommand::Key(key) | SessionCommand::KeyRelease(key) => {
                key.text.as_ref().map_or(0, String::len)
            }
            SessionCommand::Ssh { params, .. } => params.to_string().len(),
            _ => 0,
        }
    }
    pub(super) fn reserve(&self, command: &SessionCommand) -> bool {
        let bytes = Self::weight(command);
        let accepted = self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_COMMAND_BYTES)
            })
            .is_ok();
        if !accepted {
            self.reject();
        }
        accepted
    }
    pub(super) fn release(&self, command: &SessionCommand) {
        self.bytes
            .fetch_sub(Self::weight(command), Ordering::AcqRel);
    }
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub(super) fn reject(&self) {
        self.rejected.store(true, Ordering::Release);
    }
    pub(super) fn take_rejected(&self) -> bool {
        self.rejected.swap(false, Ordering::AcqRel)
    }
}

pub(super) struct TerminalEventSender {
    sender: flume::Sender<SessionEvent>,
    pub(super) control: RuntimeControl,
}

impl TerminalEventSender {
    pub(super) fn new(sender: flume::Sender<SessionEvent>, control: RuntimeControl) -> Self {
        Self { sender, control }
    }
    pub(super) fn send(&self, mut event: SessionEvent) -> Result<(), ()> {
        loop {
            if self.control.is_stopped() {
                return Err(());
            }
            match self
                .sender
                .send_timeout(event, std::time::Duration::from_millis(10))
            {
                Ok(()) => return Ok(()),
                Err(flume::SendTimeoutError::Timeout(value)) => event = value,
                Err(flume::SendTimeoutError::Disconnected(_)) => return Err(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn oversized_input_wakes_an_idle_actor_to_report_rejection() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-c", "sleep 30"]),
            TerminalSize::new(80, 24).unwrap(),
        )
        .unwrap();
        assert!(
            session
                .send(SessionCommand::Paste("x".repeat(MAX_COMMAND_BYTES + 1)))
                .is_err()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let event = session
                .events()
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if matches!(event, SessionEvent::InputRejected) {
                break;
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn shutdown_interrupts_an_actor_with_unconsumed_events() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-c", "while :; do printf '\\007'; done"]),
            TerminalSize::new(80, 24).unwrap(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(session.events().len() <= 64);
        let started = std::time::Instant::now();
        drop(session);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn budget_recovers_and_oversized_input_is_rejected_atomically() {
        let control = RuntimeControl::default();
        let command = SessionCommand::Paste("x".repeat(MAX_COMMAND_BYTES));
        assert!(control.reserve(&command));
        assert!(!control.reserve(&SessionCommand::Input(vec![1])));
        assert!(control.take_rejected());
        control.release(&command);
        assert!(control.reserve(&SessionCommand::Input(vec![1])));
    }

    #[test]
    fn stopped_event_sender_does_not_wait_for_a_full_queue() {
        let control = RuntimeControl::default();
        let (sender, _receiver) = flume::bounded(1);
        let events = TerminalEventSender::new(sender, control.clone());
        events.send(SessionEvent::Bell).unwrap();
        control.stop();
        assert!(events.send(SessionEvent::Bell).is_err());
    }
}
