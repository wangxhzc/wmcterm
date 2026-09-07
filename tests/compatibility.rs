#![cfg(unix)]

use std::time::{Duration, Instant};
use wmcterm_pty::CommandSpec;
use wmcterm_terminal::{SessionCommand, SessionEvent, TerminalSession, TerminalSize};

fn wait_for_text(session: &TerminalSession, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match session.events().recv_timeout(Duration::from_millis(100)) {
            Ok(SessionEvent::Snapshot(snapshot)) if snapshot.to_plain_text().contains(needle) => {
                return true;
            }
            Ok(SessionEvent::Error(error)) => panic!("terminal error: {error}"),
            Ok(_) | Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }
    false
}

#[test]
fn bash_runs_through_terminal_actor() {
    let session = TerminalSession::spawn(
        // A PTY makes Bash interactive: skipping startup files alone still
        // loads and saves the caller's history. Disable both history recording
        // and access to the real history file for this test shell.
        CommandSpec::new("bash")
            .args(["--noprofile", "--norc", "+o", "history"])
            .env("HISTFILE", "/dev/null"),
        TerminalSize::new(60, 10).unwrap(),
    )
    .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < ready_deadline {
        if matches!(
            session.events().recv_timeout(Duration::from_millis(100)),
            Ok(SessionEvent::Snapshot(_))
        ) {
            break;
        }
    }
    session
        .send(SessionCommand::Input(
            b"printf 'bash-compatible\\n'\r".to_vec(),
        ))
        .unwrap();
    assert!(wait_for_text(&session, "bash-compatible"));
}

#[test]
fn vim_alternate_screen_starts_and_exits() {
    if std::process::Command::new("vim")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let session = TerminalSession::spawn(
        CommandSpec::new("vim").args(["-Nu", "NONE", "-n", "-i", "NONE"]),
        TerminalSize::new(80, 24).unwrap(),
    )
    .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < ready_deadline {
        if matches!(
            session.events().recv_timeout(Duration::from_millis(100)),
            Ok(SessionEvent::Snapshot(_))
        ) {
            break;
        }
    }
    session
        .send(SessionCommand::Input(b":q!\r".to_vec()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(
            session.events().recv_timeout(Duration::from_millis(100)),
            Ok(SessionEvent::Exited(_))
        ) {
            return;
        }
    }
    panic!("vim did not exit through PTY input");
}
