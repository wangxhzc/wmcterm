use std::process::Command;

#[test]
fn ssh_help_and_invalid_targets_do_not_require_a_control_socket() {
    for args in [
        vec!["ssh", "--help"],
        vec!["ssh", "-h"],
        vec!["ssh", "exec", "--help"],
        vec!["ssh", "status", "--help"],
        vec!["ssh", "cancel", "-h"],
        vec!["ssh", "--invalid-option"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_wmcterm"))
            .args(&args)
            .env(
                "WMCTERM_CONTROL_SOCKET",
                "/nonexistent/wmcterm-ssh-cli-test.sock",
            )
            .output()
            .unwrap();
        if args.contains(&"--invalid-option") {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("SSH target must precede options")
            );
        } else {
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("wmcterm ssh HOST"));
        }
    }
}

#[test]
fn ssh_default_split_requires_a_valid_caller_pane() {
    for (args, pane, expected) in [
        (
            vec!["ssh", "example"],
            None,
            "--pane UUID or WMCTERM_PANE_ID",
        ),
        (
            vec!["ssh", "example"],
            Some("invalid"),
            "invalid SSH pane UUID",
        ),
        (
            vec!["ssh", "example", "--pane", "invalid"],
            Some("00000000-0000-0000-0000-000000000001"),
            "invalid SSH pane UUID",
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wmcterm"));
        command.args(args).env_remove("WMCTERM_PANE_ID").env(
            "WMCTERM_CONTROL_SOCKET",
            "/nonexistent/wmcterm-ssh-cli-test.sock",
        );
        if let Some(pane) = pane {
            command.env("WMCTERM_PANE_ID", pane);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
    }
}
