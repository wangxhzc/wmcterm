#![cfg(unix)]

use std::time::{Duration, Instant};
use wmcterm_pty::CommandSpec;
use wmcterm_terminal::{
    InputKey, KeyInput, KeyModifiers, SessionCommand, SessionEvent, TerminalSession, TerminalSize,
};

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

#[test]
fn kitty_keyboard_query_and_shift_enter_round_trip_through_pty() {
    // Newer Claude clients discover keyboard support via queries, including
    // over SSH where the terminal name alone may not identify capabilities.
    let script = r#"
stty raw -echo
printf '\033[?u'
IFS= read -r -d u -t 3 reply || exit 1
[[ "$reply" == $'\e[?0' ]] || exit 2
printf '\033[>1u\033[?u'
IFS= read -r -d u -t 3 reply || exit 3
[[ "$reply" == $'\e[?1' ]] || exit 4
printf 'KITTY-READY'
IFS= read -r -d u -t 3 reply || exit 5
[[ "$reply" == $'\e[13;2' ]] || exit 6
printf '\033[<u\033[?u'
IFS= read -r -d u -t 3 reply || exit 7
[[ "$reply" == $'\e[?0' ]] || exit 8
printf '\r\nKITTY-ROUNDTRIP-OK'
"#;
    let session = TerminalSession::spawn(
        CommandSpec::new("bash").args(["--noprofile", "--norc", "-c", script]),
        TerminalSize::new(80, 24).unwrap(),
    )
    .unwrap();
    assert!(wait_for_text(&session, "KITTY-READY"));
    session
        .send(SessionCommand::Key(KeyInput {
            key: InputKey::Enter,
            text: Some("\r".into()),
            modifiers: KeyModifiers {
                shift: true,
                ..KeyModifiers::default()
            },
            repeat: false,
        }))
        .unwrap();
    assert!(wait_for_text(&session, "KITTY-ROUNDTRIP-OK"));
}
