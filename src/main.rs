use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use wmcterm_core::AgentRunMode;
use wmcterm_ipc::{Request, call, default_socket_path, notify as notify_ipc};
use wmcterm_pty::{CommandSpec, PtySize, run_to_end};
use wmcterm_terminal::{TerminalEngine, TerminalSize};

const WMCTERM_SKILL: &str = include_str!("../integrations/skills/wmcterm/SKILL.md");

fn main() -> Result<()> {
    ensure_supported_windows()?;
    let mode = env::args().nth(1);
    if matches!(mode.as_deref(), Some("-h" | "--help" | "help")) {
        print_help();
        return Ok(());
    }
    if matches!(mode.as_deref(), Some("-V" | "--version")) {
        println!("wmcterm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if mode.is_none() || mode.as_deref() == Some("gui") {
        return wmcterm_app::run();
    }
    if let Some(project_dir) = mode.as_deref().filter(|value| Path::new(value).is_dir()) {
        env::set_current_dir(project_dir)
            .with_context(|| format!("could not enter project directory {project_dir}"))?;
        return wmcterm_app::run();
    }
    if mode.as_deref() == Some("rpc") {
        return run_rpc();
    }
    if mode.as_deref() == Some("codex-teams") {
        return run_codex_teams();
    }
    if matches!(
        mode.as_deref(),
        Some(
            "workspace"
                | "pane"
                | "surface"
                | "screen"
                | "send"
                | "send-line"
                | "send-key"
                | "notify"
                | "agent-event"
                | "agent-request"
                | "hook"
                | "skill"
                | "terminal"
                | "notification"
                | "ssh"
                | "docker"
                | "kube"
                | "config"
                | "window"
                | "agent"
                | "capabilities"
                | "status"
        )
    ) {
        return run_cli(mode.expect("mode checked"));
    }
    let (size, command) = parse_args()?;
    let output =
        run_to_end(&command, PtySize::new(size.cols, size.rows)).context("PTY command failed")?;

    let mut terminal = TerminalEngine::new(size).context("terminal initialization failed")?;
    terminal.feed(&output.bytes);
    let snapshot = terminal.snapshot().context("screen snapshot failed")?;

    println!("{}", snapshot.to_plain_text());
    if !output.status.success() {
        bail!("child exited with status {}", output.status);
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_supported_windows() -> Result<()> {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(version: *mut OsVersionInfo) -> i32;
    }
    let mut version = OsVersionInfo {
        size: std::mem::size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: RtlGetVersion writes the initialized, correctly sized structure
    // synchronously and retains no pointer.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status != 0 || !windows_version_supported(version.major, version.build) {
        bail!("wmcterm requires Windows 10 version 1809 (build 17763) or later");
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_version_supported(major: u32, build: u32) -> bool {
    major > 10 || (major == 10 && build >= 17_763)
}

#[cfg(not(windows))]
fn ensure_supported_windows() -> Result<()> {
    Ok(())
}

fn print_help() {
    println!(
        "wmcterm - GPU terminal workspace\n\
         \nUSAGE:\n\
         \x20 wmcterm [gui|PROJECT_DIR]\n\
         \x20 wmcterm dump [--cols N] [--rows N] [--] [COMMAND [ARG...]]\n\
         \x20 wmcterm workspace list|new|activate|rename|describe|cwd|move|pin|close ...\n\
         \x20 wmcterm pane split [--pane ID|--surface ID] [--left|--right|--up|--down] [--no-focus] [--command CMD] [--cwd DIR] [--env KEY=VALUE] [--title TITLE]\n\
         \x20 wmcterm pane close ID | close-others [--anchor ID]\n\
         \x20 wmcterm surface new [--pane ID|--surface ID] [--no-focus] [--command CMD] [--cwd DIR] [--env KEY=VALUE] [--title TITLE]\n\
         \x20 wmcterm surface markdown|logs PATH [--workspace ID|--pane ID|--surface ID] [--no-focus]\n\
         \x20 wmcterm surface close [ID] | close-others [--anchor ID]\n\
         \x20 wmcterm send [--surface ID] TEXT...\n\
         \x20 wmcterm send-line [--surface ID] TEXT...\n\
         \x20 wmcterm send-key KEY [--surface ID]\n\
         \x20 wmcterm screen read [--surface ID] [--scrollback] [--lines N]\n\
         \x20 wmcterm screen wait [--surface ID] (--until TEXT|--change|--revision N) [--timeout DURATION]\n\
         \x20 wmcterm terminal search|clear|scroll ...\n\
         \x20 wmcterm notification list|mark ID [--unread]\n\
         \x20 wmcterm notify [--title TITLE] [--body BODY] [--surface ID]\n\
         \x20 wmcterm agent-event JSON\n\
         \x20 wmcterm agent-request list|reply|cancel ...\n\
         \x20 wmcterm hook receive|plan|install --agent KIND [--event NAME]\n\
         \x20 wmcterm skill print|install [--scope project|user] [--force]\n\
         \x20 wmcterm agent [--mode interactive|unattended|full-access] claude|codex|gemini|opencode [ARG...]\n\
         \x20 wmcterm codex-teams [CODEX_ARG...]\n\
         \x20   Codex defaults: --ask-for-approval on-request --sandbox workspace-write\n\
         \x20   Unattended: Codex --approve-for-me/workspace-write; Claude --permission-mode auto\n\
         \x20   --mode full-access is blocked in CLI and requires UI-authorized flow\n\
         \x20   Managed Codex/Claude bypass options are rejected\n\
         \x20 wmcterm ssh TARGET [SSH_ARG...]\n\
         \x20 wmcterm docker CONTAINER [COMMAND...]\n\
         \x20 wmcterm kube POD [COMMAND...]\n\
         \x20 wmcterm config path|show|validate|init|import-ghostty [PATH]\n\
         \x20 wmcterm window new [PROJECT_DIR]\n\
         \x20 wmcterm capabilities\n\
         \x20 wmcterm status\n\
         \x20 wmcterm rpc METHOD [PARAMS_JSON]"
    );
}

fn run_codex_teams() -> Result<()> {
    // Reject unsafe permission shortcuts before allocating bridge resources or
    // requiring terminal context.
    let args = plan_codex_launch_args(
        AgentRunMode::Interactive,
        &env::args().skip(2).collect::<Vec<_>>(),
    )?;
    let config = codex_teams_config(
        env::var("WMCTERM_WORKSPACE_ID").ok(),
        env::var("WMCTERM_SURFACE_ID").ok(),
        env::var_os("WMCTERM_CONTROL_SOCKET").or_else(|| env::var_os("WMCTERM_SOCKET")),
        env::var("WMCTERM_CODEX_MAX_AUTO_DEPTH").ok().as_deref(),
        env::var_os("WMCTERM_CODEX_PATH"),
        wmcterm_codex::allocate_loopback_endpoint()?,
    )?;
    let codex_program = config.codex_program.clone();
    let status = match wmcterm_codex::launch_codex_teams(config, &args) {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "wmcterm: Codex app-server integration is unavailable ({error}); starting Codex without the teams bridge"
            );
            std::process::Command::new(&codex_program)
                .args(&args)
                .status()
                .with_context(|| format!("could not start Codex `{}`", codex_program.display()))?
        }
    };
    if !status.success() {
        bail!("Codex exited with status {status}");
    }
    Ok(())
}

/// Apply wmcterm's safe Codex launch policy without overriding explicit user
/// approval or sandbox configuration. Fully unsafe shortcuts are never accepted
/// through the managed multi-agent launcher.
fn plan_codex_launch_args(mode: AgentRunMode, args: &[String]) -> Result<Vec<String>> {
    if mode == AgentRunMode::FullAccess {
        bail!("`--mode full-access` is not supported for CLI launch");
    }
    let mut approval_is_explicit = false;
    let mut sandbox_is_explicit = false;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--" {
            break;
        }
        if matches!(
            argument.as_str(),
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox"
        ) || argument.starts_with("--yolo=")
            || argument.starts_with("--dangerously-bypass-approvals-and-sandbox=")
        {
            bail!(
                "unsafe Codex option {argument} is not allowed by wmcterm's managed agent launcher"
            );
        }
        match argument.as_str() {
            "-a" | "--ask-for-approval" => approval_is_explicit = true,
            "-s" | "--sandbox" => {
                if args
                    .get(index + 1)
                    .is_some_and(|value| value == "danger-full-access")
                {
                    bail!(
                        "Codex sandbox `danger-full-access` is not allowed by wmcterm's managed agent launcher"
                    );
                }
                sandbox_is_explicit = true;
            }
            // These Codex shortcuts make both permission dimensions explicit.
            "--full-auto" | "--approve-for-me" => {
                approval_is_explicit = true;
                sandbox_is_explicit = true;
            }
            "-c" | "--config" => {
                if let Some(value) = args.get(index + 1) {
                    reject_unsafe_codex_permission_config(value)?;
                    mark_codex_permission_config(
                        value,
                        &mut approval_is_explicit,
                        &mut sandbox_is_explicit,
                    );
                    index += 1;
                }
            }
            _ => {
                if argument.starts_with("--ask-for-approval=") {
                    approval_is_explicit = true;
                } else if let Some(value) = argument.strip_prefix("--sandbox=") {
                    if value == "danger-full-access" {
                        bail!(
                            "Codex sandbox `danger-full-access` is not allowed by wmcterm's managed agent launcher"
                        );
                    }
                    sandbox_is_explicit = true;
                } else if let Some(value) = argument
                    .strip_prefix("-c=")
                    .or_else(|| argument.strip_prefix("--config="))
                {
                    reject_unsafe_codex_permission_config(value)?;
                    mark_codex_permission_config(
                        value,
                        &mut approval_is_explicit,
                        &mut sandbox_is_explicit,
                    );
                }
            }
        }
        index += 1;
    }

    let mut planned = Vec::with_capacity(args.len() + 4);
    match mode {
        AgentRunMode::Interactive => {
            if !approval_is_explicit {
                planned.extend(["--ask-for-approval".into(), "on-request".into()]);
            }
            if !sandbox_is_explicit {
                planned.extend(["--sandbox".into(), "workspace-write".into()]);
            }
        }
        AgentRunMode::Unattended => {
            if sandbox_is_explicit && !approval_is_explicit {
                bail!(
                    "an explicit Codex sandbox cannot be combined with wmcterm unattended mode; `--approve-for-me` selects the workspace-write sandbox itself"
                );
            }
            if !approval_is_explicit {
                planned.extend(["--approve-for-me".into()]);
            }
        }
        AgentRunMode::FullAccess => unreachable!("full access is rejected above"),
    }
    planned.extend_from_slice(args);
    Ok(planned)
}

fn mark_codex_permission_config(value: &str, approval: &mut bool, sandbox: &mut bool) {
    let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
    match key {
        "approval_policy" => *approval = true,
        "sandbox_mode" => *sandbox = true,
        _ => {}
    }
}

fn reject_unsafe_codex_permission_config(value: &str) -> Result<()> {
    let Some((key, value)) = value.split_once('=') else {
        return Ok(());
    };
    if key.trim() == "sandbox_mode"
        && value.trim().trim_matches(['\'', '"']) == "danger-full-access"
    {
        bail!(
            "Codex sandbox `danger-full-access` is not allowed by wmcterm's managed agent launcher"
        );
    }
    Ok(())
}

fn codex_teams_config(
    workspace_id: Option<String>,
    root_surface_id: Option<String>,
    control_socket: Option<std::ffi::OsString>,
    max_auto_depth: Option<&str>,
    codex_program: Option<std::ffi::OsString>,
    endpoint: String,
) -> Result<wmcterm_codex::BridgeConfig> {
    Ok(wmcterm_codex::BridgeConfig {
        endpoint,
        control_socket: control_socket
            .map(PathBuf::from)
            .context("wmcterm control socket is unavailable")?,
        workspace_id: workspace_id
            .context("wmcterm codex-teams must run inside a wmcterm terminal")?,
        root_surface_id: root_surface_id
            .context("wmcterm codex-teams must run inside a wmcterm terminal")?,
        codex_program: codex_program
            .map(PathBuf::from)
            .unwrap_or_else(resolve_codex_program),
        max_auto_depth: max_auto_depth
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(2)
            .min(wmcterm_codex::ABSOLUTE_MAX_AUTO_DEPTH),
    })
}

#[cfg(windows)]
fn resolve_codex_program() -> PathBuf {
    for name in ["codex.exe", "codex.cmd"] {
        let Ok(output) = std::process::Command::new("where.exe").arg(name).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        for path in String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
        {
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            {
                return path;
            }
            for candidate in codex_native_candidates(&path) {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from("codex")
}

#[cfg(not(windows))]
fn resolve_codex_program() -> PathBuf {
    PathBuf::from("codex")
}

#[cfg(any(windows, test))]
fn codex_native_candidates(shim: &Path) -> Vec<PathBuf> {
    let Some(prefix) = shim.parent() else {
        return Vec::new();
    };
    let triple = "x86_64-pc-windows-msvc";
    [
        prefix.join("node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor"),
        prefix.join("node_modules/@openai/codex-win32-x64/vendor"),
        prefix.join("node_modules/@openai/codex/vendor"),
    ]
    .map(|vendor| vendor.join(triple).join("bin/codex.exe"))
    .into()
}

fn run_rpc() -> Result<()> {
    let mut args = env::args().skip(2);
    let method = args
        .next()
        .context("usage: wmcterm rpc METHOD [PARAMS_JSON]")?;
    let params = match args.next() {
        Some(value) => serde_json::from_str(&value).context("invalid params JSON")?,
        None => Value::Null,
    };
    print_response(rpc(&method, params)?)
}

fn rpc(method: &str, params: Value) -> Result<wmcterm_ipc::Response> {
    let path = control_socket_path()?;
    Ok(call(
        &path,
        &Request {
            id: "cli".into(),
            method: method.into(),
            params,
        },
    )?)
}

fn control_socket_path() -> Result<PathBuf> {
    env::var_os("WMCTERM_CONTROL_SOCKET")
        .or_else(|| env::var_os("WMCTERM_SOCKET"))
        .map(PathBuf::from)
        .or_else(default_socket_path)
        .context("could not determine wmcterm socket path")
}

fn print_response(response: wmcterm_ipc::Response) -> Result<()> {
    if let Some(error) = response.error {
        bail!("RPC {}: {}", error.code, error.message);
    }
    println!("{}", serde_json::to_string_pretty(&response.result)?);
    Ok(())
}

fn run_cli(scope: String) -> Result<()> {
    let mut args = env::args().skip(2);
    let response = match scope.as_str() {
        "workspace" => match args.next().as_deref() {
            Some("list") => rpc("workspace.list", Value::Null)?,
            Some("new") => {
                let mut name = "workspace".to_owned();
                let mut cwd = env::current_dir()?;
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--name" => name = args.next().context("--name requires a value")?,
                        "--cwd" => cwd = args.next().context("--cwd requires a value")?.into(),
                        value => bail!("unknown workspace option: {value}"),
                    }
                }
                cwd = wmcterm_config::expand_home_path(&cwd)
                    .canonicalize()
                    .with_context(|| {
                        format!("workspace directory does not exist: {}", cwd.display())
                    })?;
                if !cwd.is_dir() {
                    bail!("workspace path is not a directory: {}", cwd.display());
                }
                rpc(
                    "workspace.create",
                    serde_json::json!({"name": name, "cwd": cwd}),
                )?
            }
            Some(operation @ ("activate" | "close")) => {
                let id = args
                    .next()
                    .with_context(|| format!("workspace {operation} requires ID"))?;
                rpc(
                    &format!("workspace.{operation}"),
                    serde_json::json!({"id": id}),
                )?
            }
            Some(operation @ ("rename" | "describe")) => {
                let id = args
                    .next()
                    .with_context(|| format!("workspace {operation} requires ID"))?;
                let value = args.collect::<Vec<_>>().join(" ");
                let key = if operation == "rename" {
                    "name"
                } else {
                    "description"
                };
                rpc(
                    &format!("workspace.{operation}"),
                    serde_json::json!({"id": id, key: value}),
                )?
            }
            Some("cwd") => {
                let id = args.next().context("workspace cwd requires ID")?;
                let cwd = args.next().context("workspace cwd requires PATH")?;
                if args.next().is_some() {
                    bail!("workspace cwd accepts exactly ID and PATH");
                }
                rpc("workspace.cwd", serde_json::json!({"id": id, "cwd": cwd}))?
            }
            Some("move") => {
                let id = args.next().context("workspace move requires ID")?;
                let offset = args
                    .next()
                    .context("workspace move requires OFFSET")?
                    .parse::<isize>()?;
                rpc(
                    "workspace.move",
                    serde_json::json!({"id": id, "offset": offset}),
                )?
            }
            Some("pin") => {
                let id = args.next().context("workspace pin requires ID")?;
                let pinned = !args.any(|value| value == "--off");
                rpc(
                    "workspace.pin",
                    serde_json::json!({"id": id, "pinned": pinned}),
                )?
            }
            _ => bail!(
                "usage: wmcterm workspace list|new|activate|rename|describe|cwd|move|pin|close"
            ),
        },
        "pane" => match args.next().as_deref() {
            Some("list") => rpc(
                "pane.list",
                serde_json::json!({"workspace": originating_workspace()}),
            )?,
            Some("split") => {
                let mut direction = "right";
                let mut command = None;
                let mut pane = None;
                let mut surface = originating_surface();
                let mut focus = true;
                let mut cwd = None;
                let mut title = None;
                let mut environment = serde_json::Map::new();
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--down" => direction = "down",
                        "--right" => direction = "right",
                        "--up" => direction = "up",
                        "--left" => direction = "left",
                        "--direction" => {
                            direction = match args
                                .next()
                                .context("--direction requires a value")?
                                .as_str()
                            {
                                "right" => "right",
                                "down" => "down",
                                "left" => "left",
                                "up" => "up",
                                value => bail!("invalid split direction: {value}"),
                            }
                        }
                        "--pane" => pane = Some(args.next().context("--pane requires an ID")?),
                        "--surface" => {
                            surface = Some(args.next().context("--surface requires an ID")?)
                        }
                        "--focus" => focus = true,
                        "--no-focus" => focus = false,
                        "--cwd" => cwd = Some(args.next().context("--cwd requires a value")?),
                        "--title" => title = Some(args.next().context("--title requires a value")?),
                        "--env" => {
                            let (key, value) = parse_environment_assignment(
                                &args.next().context("--env requires KEY=VALUE")?,
                            )?;
                            environment.insert(key, value.into());
                        }
                        "--command" => {
                            command = Some(args.next().context("--command requires a value")?)
                        }
                        value => bail!("unknown pane split option: {value}"),
                    }
                }
                let workspace = (pane.is_none() && surface.is_none())
                    .then(originating_workspace)
                    .flatten();
                rpc(
                    "pane.split",
                    serde_json::json!({
                        "direction": direction, "pane": pane, "surface": surface,
                        "workspace": workspace,
                        "focus": focus, "command": command, "cwd": cwd,
                        "environment": environment, "title": title
                    }),
                )?
            }
            Some("close-others") => {
                let anchor = parse_anchor(&mut args, originating_pane(), "pane")?;
                rpc("pane.close-others", serde_json::json!({"anchor": anchor}))?
            }
            Some(operation @ ("focus" | "zoom")) => {
                let id = args
                    .next()
                    .with_context(|| format!("pane {operation} requires ID"))?;
                rpc(&format!("pane.{operation}"), serde_json::json!({"id": id}))?
            }
            Some("close") => {
                let id = args
                    .next()
                    .or_else(originating_pane)
                    .context("pane close requires ID or WMCTERM_PANE_ID")?;
                rpc("pane.close", serde_json::json!({"id": id}))?
            }
            _ => bail!("usage: wmcterm pane list|split|focus|zoom|close|close-others"),
        },
        "screen" => match args.next().as_deref() {
            Some("read") => {
                let options = parse_screen_options(args)?;
                rpc(
                    "screen.read",
                    serde_json::json!({
                        "surface": options.surface,
                        "scrollback": options.scrollback,
                        "lines": options.lines,
                    }),
                )?
            }
            Some("wait") => return run_screen_wait(args),
            _ => bail!("usage: wmcterm screen read|wait ..."),
        },
        "send" => {
            let mut surface = originating_surface();
            let mut text = Vec::new();
            while let Some(value) = args.next() {
                if value == "--" {
                    text.extend(args);
                    break;
                } else if value == "--surface" {
                    surface = Some(args.next().context("--surface requires an ID")?);
                } else {
                    text.push(value);
                }
            }
            rpc(
                "send",
                serde_json::json!({
                    "text": text.join(" "),
                    "surface": surface,
                    "paste": true,
                }),
            )?
        }
        "send-line" => {
            let (surface, text) = parse_send_text(args)?;
            rpc(
                "send.line",
                serde_json::json!({
                    "text": text,
                    "surface": surface,
                }),
            )?
        }
        "send-key" => {
            let key = args.next().context("usage: wmcterm send-key KEY")?;
            let surface = parse_optional_target(&mut args, "--surface")?;
            rpc(
                "send",
                serde_json::json!({
                    "text": encode_control_key(&key)?,
                    "surface": surface,
                    "paste": false,
                }),
            )?
        }
        "notify" => {
            let mut title = "wmcterm".to_owned();
            let mut body = String::new();
            let mut surface = originating_surface();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--title" => title = args.next().context("--title requires a value")?,
                    "--body" => body = args.next().context("--body requires a value")?,
                    "--surface" => surface = Some(args.next().context("--surface requires an ID")?),
                    value => bail!("unknown notify option: {value}"),
                }
            }
            rpc(
                "notify",
                serde_json::json!({
                    "title": title,
                    "body": body,
                    "surface": surface,
                }),
            )?
        }
        "agent-event" => {
            let mut params: Value =
                serde_json::from_str(&args.next().context("usage: wmcterm agent-event JSON")?)?;
            if params.get("event").is_some_and(Value::is_object) {
                params = wmcterm_agent::AgentEventEnvelope::from_compatible_value(params)?
                    .to_legacy_rpc_params()?;
            }
            if let (Some(surface), Some(object)) = (originating_surface(), params.as_object_mut()) {
                object.entry("surface").or_insert_with(|| surface.into());
            }
            rpc("agent.update", params)?
        }
        "agent-request" => match args.next().as_deref() {
            Some("list") => rpc("agent.request.list", Value::Null)?,
            Some("reply") => {
                let request_id = args.next().context("agent-request reply requires ID")?;
                let mut resolution = serde_json::json!({});
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--approve" => resolution["approved"] = true.into(),
                        "--deny" => resolution["approved"] = false.into(),
                        "--choice" => {
                            resolution["choice_id"] =
                                args.next().context("--choice requires an ID")?.into()
                        }
                        "--text" => {
                            resolution["text"] =
                                args.next().context("--text requires a value")?.into()
                        }
                        value => bail!("unknown agent request reply option: {value}"),
                    }
                }
                rpc(
                    "agent.request.reply",
                    serde_json::json!({"request_id": request_id, "resolution": resolution}),
                )?
            }
            Some("cancel") => {
                let request_id = args.next().context("agent-request cancel requires ID")?;
                let reason = args.collect::<Vec<_>>().join(" ");
                rpc(
                    "agent.request.cancel",
                    serde_json::json!({"request_id": request_id, "reason": reason}),
                )?
            }
            _ => bail!("usage: wmcterm agent-request list|reply|cancel"),
        },
        "hook" => return run_hook(args),
        "skill" => return run_skill(args),
        "agent" => {
            return run_agent(args);
        }
        "status" => rpc("app.status", Value::Null)?,
        "capabilities" => rpc("app.capabilities", Value::Null)?,
        "config" => return run_config(args),
        "window" if args.next().as_deref() == Some("new") => {
            let mut command = std::process::Command::new(env::current_exe()?);
            if let Some(project) = args.next() {
                command.arg(project);
            } else {
                command.arg("gui");
            }
            command.spawn().context("starting wmcterm window")?;
            return Ok(());
        }
        "ssh" => {
            let target = args
                .next()
                .context("usage: wmcterm ssh TARGET [SSH_ARG...]")?;
            let rest: Vec<_> = args.collect();
            let command = if rest.first().is_some_and(|arg| arg == "--tmux") {
                let session = rest.get(1).cloned().unwrap_or_else(|| "wmcterm".into());
                format!(
                    "ssh -t {} {}",
                    shell_arg(&target),
                    shell_arg(&format!(
                        "tmux attach -t {session} || tmux new -s {session}"
                    ))
                )
            } else {
                std::iter::once("ssh".to_owned())
                    .chain(std::iter::once(target.clone()))
                    .chain(rest)
                    .map(|part| shell_arg(&part))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            rpc(
                "workspace.create",
                serde_json::json!({"name": target, "cwd": env::current_dir()?, "command": command}),
            )?
        }
        "docker" | "kube" => {
            let target = args
                .next()
                .with_context(|| format!("usage: wmcterm {scope} TARGET [COMMAND...]"))?;
            let mut command = if scope == "docker" {
                vec!["docker".into(), "exec".into(), "-it".into(), target]
            } else {
                vec![
                    "kubectl".into(),
                    "exec".into(),
                    "-it".into(),
                    target,
                    "--".into(),
                ]
            };
            let rest: Vec<_> = args.collect();
            command.extend(if rest.is_empty() {
                vec!["sh".into()]
            } else {
                rest
            });
            rpc(
                "surface.new",
                serde_json::json!({
                    "command": command.iter().map(|part| shell_arg(part)).collect::<Vec<_>>().join(" "),
                    "workspace": originating_workspace(),
                }),
            )?
        }
        "surface" => match args.next().as_deref() {
            Some("list") => rpc(
                "surface.list",
                serde_json::json!({"workspace": originating_workspace()}),
            )?,
            Some("new") => {
                let mut command = None;
                let mut pane = None;
                let mut surface = originating_surface();
                let mut focus = true;
                let mut cwd = None;
                let mut title = None;
                let mut environment = serde_json::Map::new();
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--pane" => pane = Some(args.next().context("--pane requires an ID")?),
                        "--surface" => {
                            surface = Some(args.next().context("--surface requires an ID")?)
                        }
                        "--focus" => focus = true,
                        "--no-focus" => focus = false,
                        "--cwd" => cwd = Some(args.next().context("--cwd requires a value")?),
                        "--title" => title = Some(args.next().context("--title requires a value")?),
                        "--env" => {
                            let (key, value) = parse_environment_assignment(
                                &args.next().context("--env requires KEY=VALUE")?,
                            )?;
                            environment.insert(key, value.into());
                        }
                        "--command" => {
                            command = Some(args.next().context("--command requires a value")?)
                        }
                        value => bail!("unknown surface option: {value}"),
                    }
                }
                let workspace = (pane.is_none() && surface.is_none())
                    .then(originating_workspace)
                    .flatten();
                rpc(
                    "surface.new",
                    serde_json::json!({
                        "pane": pane, "surface": surface, "focus": focus,
                        "workspace": workspace,
                        "command": command, "cwd": cwd,
                        "environment": environment, "title": title
                    }),
                )?
            }
            Some("close") => {
                let id = args
                    .next()
                    .or_else(originating_surface)
                    .context("surface close requires ID or WMCTERM_SURFACE_ID")?;
                rpc("surface.close", serde_json::json!({"id": id}))?
            }
            Some("close-others") => {
                let anchor = parse_anchor(&mut args, originating_surface(), "surface")?;
                rpc(
                    "surface.close-others",
                    serde_json::json!({"anchor": anchor}),
                )?
            }
            Some("activate") => rpc(
                "surface.activate",
                serde_json::json!({"id": args.next().context("surface activate requires ID")?}),
            )?,
            Some(kind @ ("markdown" | "logs")) => {
                let path = args
                    .next()
                    .with_context(|| format!("surface {kind} requires PATH"))?;
                let mut pane = None;
                let mut surface = originating_surface();
                let mut workspace = None;
                let mut focus = true;
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--pane" => {
                            pane = Some(args.next().context("--pane requires ID")?);
                            surface = None;
                        }
                        "--surface" => {
                            surface = Some(args.next().context("--surface requires ID")?)
                        }
                        "--workspace" => {
                            workspace = Some(args.next().context("--workspace requires ID")?);
                            pane = None;
                            surface = None;
                        }
                        "--focus" => focus = true,
                        "--no-focus" => focus = false,
                        value => bail!("unknown surface {kind} option: {value}"),
                    }
                }
                if pane.is_none() && surface.is_none() && workspace.is_none() {
                    workspace = originating_workspace();
                }
                rpc(
                    &format!("surface.{kind}"),
                    serde_json::json!({
                        "path": path, "workspace": workspace, "pane": pane,
                        "surface": surface, "focus": focus,
                    }),
                )?
            }
            Some("read") => rpc(
                "surface.read",
                serde_json::json!({"id": args.next().or_else(originating_surface)}),
            )?,
            Some("search") => {
                let query = args.next().context("surface search requires QUERY")?;
                rpc(
                    "surface.search",
                    serde_json::json!({"query": query, "id": args.next().or_else(originating_surface)}),
                )?
            }
            _ => bail!("usage: wmcterm surface list|new|activate|close"),
        },
        "terminal" => match args.next().as_deref() {
            Some("search") => {
                let query = args.next().context("terminal search requires QUERY")?;
                let mut direction = 1;
                let mut surface = originating_surface();
                while let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--previous" => direction = -1,
                        "--surface" => {
                            surface = Some(args.next().context("--surface requires ID")?)
                        }
                        value => bail!("unknown terminal search option: {value}"),
                    }
                }
                rpc(
                    "terminal.search",
                    serde_json::json!({"query": query, "direction": direction, "surface": surface}),
                )?
            }
            Some("clear") => rpc(
                "terminal.clear",
                serde_json::json!({"surface": parse_optional_target(&mut args, "--surface")?}),
            )?,
            Some("scroll") => {
                let lines = args
                    .next()
                    .context("terminal scroll requires LINES")?
                    .parse::<isize>()?;
                rpc(
                    "terminal.scroll",
                    serde_json::json!({"lines": lines, "surface": parse_optional_target(&mut args, "--surface")?}),
                )?
            }
            _ => bail!("usage: wmcterm terminal search|clear|scroll"),
        },
        "notification" => match args.next().as_deref() {
            Some("list") => rpc("notification.list", Value::Null)?,
            Some("mark") => {
                let id = args.next().context("notification mark requires ID")?;
                let read = !args.any(|value| value == "--unread");
                rpc(
                    "notification.mark",
                    serde_json::json!({"id": id, "read": read}),
                )?
            }
            _ => bail!("usage: wmcterm notification list|mark ID [--unread]"),
        },
        _ => bail!("invalid command"),
    };
    print_response(response)
}

/// Run an Agent in the terminal that invoked this command.
///
/// `pane split --command "wmcterm agent claude"` already allocates the target
/// terminal. Creating another surface here used to leave that terminal empty
/// and route the Agent to the workspace's active pane when the split was made
/// with `--no-focus`.
fn run_agent(mut args: impl Iterator<Item = String>) -> Result<()> {
    let (mode, program, supplied) = parse_agent_launch_args(&mut args)?;
    let (executable, child_args) = plan_agent_process(mode, &program, &supplied)?;
    let status = std::process::Command::new(&executable)
        .args(&child_args)
        .status()
        .with_context(|| format!("could not start managed Agent `{program}`"))?;
    if !status.success() {
        bail!("managed Agent `{program}` exited with status {status}");
    }
    Ok(())
}

fn parse_agent_launch_args(
    args: &mut impl Iterator<Item = String>,
) -> Result<(AgentRunMode, String, Vec<String>)> {
    let mut args = args.collect::<Vec<_>>();
    if args.is_empty() {
        bail!("usage: wmcterm agent [--mode interactive|unattended|full-access] PROGRAM [ARG...]");
    }

    let mut mode = AgentRunMode::Interactive;
    if args[0] == "--mode" {
        let value = args
            .get(1)
            .context("`--mode` requires a mode value: interactive|unattended|full-access")?;
        mode = parse_agent_run_mode(value)?;
        args.drain(0..2);
    } else if let Some(raw) = args[0].strip_prefix("--mode=") {
        mode = parse_agent_run_mode(raw)?;
        args.remove(0);
    }

    let program = args.first().cloned().context(
        "usage: wmcterm agent [--mode interactive|unattended|full-access] PROGRAM [ARG...]",
    )?;
    let supplied = args[1..].to_vec();
    Ok((mode, program, supplied))
}

fn parse_agent_run_mode(raw: &str) -> Result<AgentRunMode> {
    AgentRunMode::from_str(raw).with_context(|| {
        format!("invalid agent run mode `{raw}`; expected interactive|unattended|full-access")
    })
}

fn plan_agent_process(
    mode: AgentRunMode,
    program: &str,
    supplied: &[String],
) -> Result<(PathBuf, Vec<String>)> {
    if mode == AgentRunMode::FullAccess {
        bail!("`--mode full-access` is only available from wmcterm's confirmed UI action");
    }
    let (executable, child_args) = if program == "codex" {
        let mut child_args = vec!["codex-teams".to_owned()];
        child_args.extend(plan_codex_launch_args(mode, supplied)?);
        (env::current_exe()?, child_args)
    } else if program == "claude" {
        (
            PathBuf::from(program),
            plan_claude_launch_args(mode, supplied)?,
        )
    } else if mode == AgentRunMode::Interactive {
        (PathBuf::from(program), supplied.to_vec())
    } else {
        bail!("Agent `{program}` does not define a safe `{mode}` launch profile")
    };
    Ok((executable, child_args))
}

fn plan_claude_launch_args(mode: AgentRunMode, args: &[String]) -> Result<Vec<String>> {
    let mut permission_mode_is_explicit = false;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--" {
            break;
        }
        if argument == "--dangerously-skip-permissions"
            || argument == "--allow-dangerously-skip-permissions"
            || argument.starts_with("--dangerously-skip-permissions=")
            || argument.starts_with("--allow-dangerously-skip-permissions=")
        {
            bail!(
                "unsafe Claude option {argument} is not allowed by wmcterm's managed agent launcher"
            );
        }
        if argument == "--permission-mode" {
            let value = args
                .get(index + 1)
                .context("`--permission-mode` requires a value")?;
            if value == "bypassPermissions" {
                bail!(
                    "Claude permission mode `bypassPermissions` is not allowed by wmcterm's managed agent launcher"
                );
            }
            permission_mode_is_explicit = true;
            index += 1;
        } else if let Some(value) = argument.strip_prefix("--permission-mode=") {
            if value == "bypassPermissions" {
                bail!(
                    "Claude permission mode `bypassPermissions` is not allowed by wmcterm's managed agent launcher"
                );
            }
            permission_mode_is_explicit = true;
        }
        index += 1;
    }

    let mut planned = Vec::with_capacity(args.len() + 2);
    if mode == AgentRunMode::Unattended && !permission_mode_is_explicit {
        planned.extend(["--permission-mode".into(), "auto".into()]);
    }
    planned.extend_from_slice(args);
    Ok(planned)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ScreenOptions {
    surface: Option<String>,
    scrollback: bool,
    lines: Option<usize>,
}

fn parse_screen_options(args: impl Iterator<Item = String>) -> Result<ScreenOptions> {
    let mut options = ScreenOptions {
        surface: originating_surface(),
        ..ScreenOptions::default()
    };
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--surface" => options.surface = Some(args.next().context("--surface requires an ID")?),
            "--scrollback" => options.scrollback = true,
            "--lines" => {
                let lines = args
                    .next()
                    .context("--lines requires a value")?
                    .parse::<usize>()
                    .context("--lines must be a positive integer")?;
                if lines == 0 {
                    bail!("--lines must be greater than zero");
                }
                options.lines = Some(lines);
            }
            value => bail!("unknown screen option: {value}"),
        }
    }
    Ok(options)
}

#[derive(Debug, PartialEq, Eq)]
enum ScreenWaitCondition {
    Until(String),
    Change,
    Revision(u64),
}

fn run_screen_wait(args: impl Iterator<Item = String>) -> Result<()> {
    let mut surface = originating_surface();
    let mut scrollback = false;
    let mut lines = None;
    let mut condition = None;
    let mut timeout = Duration::from_secs(10);
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--surface" => surface = Some(args.next().context("--surface requires an ID")?),
            "--scrollback" => scrollback = true,
            "--lines" => {
                let value = args.next().context("--lines requires a value")?;
                let parsed = value
                    .parse::<usize>()
                    .context("--lines must be a positive integer")?;
                if parsed == 0 {
                    bail!("--lines must be greater than zero");
                }
                lines = Some(parsed);
            }
            "--until" => set_wait_condition(
                &mut condition,
                ScreenWaitCondition::Until(args.next().context("--until requires text")?),
            )?,
            "--change" => set_wait_condition(&mut condition, ScreenWaitCondition::Change)?,
            "--revision" => set_wait_condition(
                &mut condition,
                ScreenWaitCondition::Revision(
                    args.next()
                        .context("--revision requires a value")?
                        .parse::<u64>()
                        .context("--revision must be an integer")?,
                ),
            )?,
            "--timeout" => {
                timeout = parse_cli_duration(&args.next().context("--timeout requires a value")?)?
            }
            value => bail!("unknown screen wait option: {value}"),
        }
    }
    let condition = condition.context("screen wait requires --until, --change, or --revision")?;
    let params = serde_json::json!({
        "surface": surface,
        "scrollback": scrollback,
        "lines": lines,
    });
    let started = std::time::Instant::now();
    let mut baseline = None;
    loop {
        let response = rpc("screen.read", params.clone())?;
        let result = response_result(&response)?;
        let text = result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let revision = result
            .get("revision")
            .and_then(Value::as_u64)
            .context("screen.read response is missing revision")?;
        let matched = match &condition {
            ScreenWaitCondition::Until(needle) => text.contains(needle),
            ScreenWaitCondition::Change => match baseline {
                Some(initial) => revision != initial,
                None => {
                    baseline = Some(revision);
                    false
                }
            },
            ScreenWaitCondition::Revision(previous) => revision != *previous,
        };
        if matched {
            return print_response(response);
        }
        if started.elapsed() >= timeout {
            bail!(
                "timed out after {} ms waiting for terminal screen",
                timeout.as_millis()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn set_wait_condition(
    target: &mut Option<ScreenWaitCondition>,
    condition: ScreenWaitCondition,
) -> Result<()> {
    if target.replace(condition).is_some() {
        bail!("screen wait accepts exactly one of --until, --change, or --revision");
    }
    Ok(())
}

fn parse_cli_duration(value: &str) -> Result<Duration> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else {
        (value, 1_000)
    };
    let milliseconds = number
        .parse::<u64>()
        .with_context(|| format!("invalid duration: {value}"))?
        .checked_mul(multiplier)
        .context("duration is too large")?;
    if milliseconds == 0 || milliseconds > 300_000 {
        bail!("duration must be between 1 ms and 300 s");
    }
    Ok(Duration::from_millis(milliseconds))
}

fn response_result(response: &wmcterm_ipc::Response) -> Result<&Value> {
    if let Some(error) = &response.error {
        bail!("RPC {}: {}", error.code, error.message);
    }
    response
        .result
        .as_ref()
        .context("RPC response has no result")
}

fn parse_send_text(args: impl Iterator<Item = String>) -> Result<(Option<String>, String)> {
    let mut surface = originating_surface();
    let mut text = Vec::new();
    let mut args = args.peekable();
    while let Some(value) = args.next() {
        if value == "--" {
            text.extend(args);
            break;
        } else if value == "--surface" {
            surface = Some(args.next().context("--surface requires an ID")?);
        } else {
            text.push(value);
        }
    }
    if text.is_empty() {
        bail!("send-line requires text");
    }
    Ok((surface, text.join(" ")))
}

fn parse_optional_target(
    args: &mut impl Iterator<Item = String>,
    flag_name: &str,
) -> Result<Option<String>> {
    let Some(flag) = args.next() else {
        return Ok(originating_surface());
    };
    if flag != flag_name {
        bail!("unknown option: {flag}");
    }
    Ok(Some(
        args.next()
            .with_context(|| format!("{flag_name} requires ID"))?,
    ))
}

fn parse_environment_assignment(value: &str) -> Result<(String, String)> {
    let (key, value) = value.split_once('=').context("--env requires KEY=VALUE")?;
    if key.is_empty() {
        bail!("--env key must not be empty");
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn originating_surface() -> Option<String> {
    env::var("WMCTERM_SURFACE_ID")
        .ok()
        .filter(|surface| !surface.trim().is_empty())
}

fn originating_pane() -> Option<String> {
    env::var("WMCTERM_PANE_ID")
        .ok()
        .filter(|pane| !pane.trim().is_empty())
}

fn originating_workspace() -> Option<String> {
    env::var("WMCTERM_WORKSPACE_ID")
        .ok()
        .filter(|workspace| !workspace.trim().is_empty())
}

fn parse_anchor(
    args: &mut impl Iterator<Item = String>,
    implicit: Option<String>,
    kind: &str,
) -> Result<String> {
    match args.next() {
        None => implicit.with_context(|| {
            format!(
                "{kind} close-others requires --anchor ID or WMCTERM_{}_ID",
                kind.to_ascii_uppercase()
            )
        }),
        Some(flag) if flag == "--anchor" => args
            .next()
            .with_context(|| format!("{kind} close-others --anchor requires ID")),
        Some(value) => bail!("unknown {kind} close-others option: {value}"),
    }
}

fn encode_control_key(key: &str) -> Result<String> {
    let normalized = key.trim().to_ascii_lowercase();
    let bytes: Vec<u8> = match normalized.as_str() {
        "enter" | "return" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "escape" | "esc" => vec![0x1b],
        "backspace" => vec![0x7f],
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        value if value.starts_with("ctrl+") || value.starts_with('^') => {
            let value = value
                .strip_prefix("ctrl+")
                .or_else(|| value.strip_prefix('^'))
                .unwrap_or_default();
            let byte = match value {
                "space" | "@" => 0,
                "[" => 27,
                "\\" => 28,
                "]" => 29,
                "^" => 30,
                "_" => 31,
                value if value.len() == 1 => {
                    let byte = value.as_bytes()[0].to_ascii_uppercase();
                    if !byte.is_ascii_alphabetic() {
                        bail!("unsupported control key: {key}");
                    }
                    byte - b'A' + 1
                }
                _ => bail!("unsupported control key: {key}"),
            };
            vec![byte]
        }
        _ => key.as_bytes().to_vec(),
    };
    String::from_utf8(bytes).context("control key encoding was not UTF-8")
}

fn run_hook(mut args: impl Iterator<Item = String>) -> Result<()> {
    use wmcterm_agent::{
        AgentEventEnvelope, AgentKind, HookConfigFormat, hook_install_plan,
        normalize_claude_hook_event, normalize_hook_event, read_agent_process_identity,
    };
    let operation = args
        .next()
        .context("usage: wmcterm hook receive|plan|install --agent KIND")?;
    let mut agent = None;
    let mut event = None;
    let mut surface = env::var("WMCTERM_SURFACE_ID").ok();
    let mut scope = InstallScope::Project;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--agent" => {
                agent = Some(
                    args.next()
                        .context("--agent requires a value")?
                        .parse::<AgentKind>()?,
                )
            }
            "--event" => event = Some(args.next().context("--event requires a value")?),
            "--surface" => surface = Some(args.next().context("--surface requires a value")?),
            "--scope" => {
                scope = InstallScope::parse(&args.next().context("--scope requires a value")?)?
            }
            value => bail!("unknown hook option: {value}"),
        }
    }
    let agent = agent.context("--agent is required")?;
    match operation.as_str() {
        "plan" => println!(
            "{}",
            serde_json::to_string_pretty(&hook_install_plan(
                agent,
                env::current_exe()?.display().to_string()
            ))?
        ),
        "receive" => {
            let inside_wmcterm = env::var_os("WMCTERM").is_some();
            let explicitly_permission = agent == AgentKind::Claude
                && event
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case("PermissionRequest"));
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                if explicitly_permission && inside_wmcterm {
                    print_claude_permission_result(wmcterm_claude::deny_response(
                        "wmcterm could not read the permission request",
                    ));
                } else {
                    acknowledge_hook();
                }
                return Ok(());
            }
            let Ok(payload) = serde_json::from_str::<Value>(&input) else {
                if explicitly_permission && inside_wmcterm {
                    print_claude_permission_result(wmcterm_claude::deny_response(
                        "wmcterm received invalid permission JSON",
                    ));
                } else {
                    acknowledge_hook();
                }
                return Ok(());
            };
            let is_claude_permission =
                agent == AgentKind::Claude && claude_permission_event(event.as_deref(), &payload);
            let bridged_codex_permission = suppress_bridged_codex_permission_hook(
                agent,
                event.as_deref(),
                &payload,
                env::var(wmcterm_codex::APPROVAL_BRIDGE_ENV).ok().as_deref()
                    == Some(wmcterm_codex::APPROVAL_BRIDGE_ENV_VALUE),
            );
            if bridged_codex_permission {
                // The managed app-server watcher owns the actionable approval.
                // Delivering this telemetry hook as a second request creates a
                // random `permission:*` entry with no corresponding resolution.
                acknowledge_hook();
                return Ok(());
            }
            if is_claude_permission {
                // A user-level Claude hook also runs outside wmcterm. In that
                // case it must not replace Claude's native permission UI.
                if !inside_wmcterm {
                    acknowledge_hook();
                    return Ok(());
                }
                let context = nonempty_env("WMCTERM_WORKSPACE_ID")
                    .zip(surface.clone())
                    .zip(nonempty_env("WMCTERM_TERMINAL_LIFECYCLE_ID"))
                    .and_then(|((workspace, surface), lifecycle)| {
                        control_socket_path()
                            .ok()
                            .map(|socket| (socket, workspace, surface, lifecycle))
                    });
                let outcome = context.map_or_else(
                    || wmcterm_claude::HookOutcome {
                        output: wmcterm_claude::deny_response(
                            "Claude permission hook is not bound to a wmcterm terminal lifecycle",
                        ),
                        diagnostic: Some("missing wmcterm hook control context".into()),
                    },
                    |(socket, workspace, surface, lifecycle)| {
                        wmcterm_claude::handle_permission_request(
                            &payload,
                            &wmcterm_claude::BridgeConfig::new(
                                socket, workspace, surface, lifecycle,
                            ),
                        )
                    },
                );
                if let Some(diagnostic) = outcome.diagnostic {
                    eprintln!("wmcterm: Claude permission denied: {diagnostic}");
                }
                print_claude_permission_result(outcome.output);
                return Ok(());
            }
            let claude_hook = (agent == AgentKind::Claude)
                .then(|| normalize_claude_hook_event(event.as_deref(), &payload).ok())
                .flatten();
            // Non-blocking lifecycle hooks acknowledge Claude before the
            // bounded telemetry delivery. PermissionRequest is handled above
            // because Claude must receive the user's actual decision.
            acknowledge_hook();
            let normalized = match claude_hook.as_ref() {
                Some(hook) => hook.to_agent_event(),
                None => match normalize_hook_event(agent, event.as_deref(), &payload) {
                    Ok(event) => event,
                    Err(_) => return Ok(()),
                },
            };
            let mut envelope = AgentEventEnvelope::from_hook_payload(normalized, &payload, surface);
            envelope.lifecycle_id = envelope
                .lifecycle_id
                .or_else(|| nonempty_env("WMCTERM_TERMINAL_LIFECYCLE_ID"))
                .or_else(|| nonempty_env("WMCTERM_LIFECYCLE_ID"));
            envelope.agent_pid = envelope
                .agent_pid
                .or_else(|| parsed_env("WMCTERM_AGENT_PID"));
            envelope.pid_start_time = envelope
                .pid_start_time
                .or_else(|| parsed_env("WMCTERM_PID_START_TIME"));
            if envelope.agent_pid.is_some() && envelope.pid_start_time.is_none() {
                let observed = envelope
                    .agent_pid
                    .and_then(|pid| read_agent_process_identity(pid).ok())
                    .filter(|identity| {
                        envelope.surface_id.as_deref() == Some(identity.surface_id.as_str())
                            && envelope.lifecycle_id.as_deref()
                                == Some(identity.lifecycle_id.as_str())
                    });
                if let Some(identity) = observed {
                    envelope.pid_start_time = Some(identity.pid_start_time);
                } else {
                    envelope.agent_pid = None;
                }
            }
            envelope.sequence = envelope
                .sequence
                .or_else(|| parsed_env("WMCTERM_HOOK_SEQUENCE"));
            // A missing surface means this hook ran outside wmcterm. Never
            // guess the active surface, because that could update another agent.
            if envelope.surface_id.is_some() {
                deliver_hook_bounded(envelope, claude_hook);
            }
        }
        "install" => {
            let plan = hook_install_plan(agent, env::current_exe()?.display().to_string());
            let snippet = plan
                .snippets
                .first()
                .context("agent does not provide a hook configuration")?;
            if snippet.format != HookConfigFormat::Json {
                bail!(
                    "automatic hook installation is not available for {agent}; use `wmcterm hook plan --agent {agent}`"
                );
            }
            let relative = snippet
                .relative_path
                .as_deref()
                .context("agent hook configuration path is unknown")?;
            let root = agent_config_root(agent, scope)?;
            let target = root.join(relative);
            let fragment: Value = serde_json::from_str(&snippet.content)?;
            let removed = install_json_fragment(&target, fragment, agent)?;
            if removed > 0 {
                println!("removed {removed} stale {agent} wmcterm hook entries");
            }
            println!("installed {} hooks in {}", agent, target.display());
            if agent == AgentKind::Codex {
                println!("review and trust the hook in Codex with /hooks");
            }
        }
        _ => bail!("usage: wmcterm hook receive|plan|install --agent KIND"),
    }
    Ok(())
}

const HOOK_DELIVERY_BUDGET: Duration = Duration::from_millis(75);

fn claude_permission_event(event: Option<&str>, payload: &Value) -> bool {
    event.is_some_and(|name| name.eq_ignore_ascii_case("PermissionRequest"))
        || payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("PermissionRequest"))
}

fn suppress_bridged_codex_permission_hook(
    agent: wmcterm_agent::AgentKind,
    event: Option<&str>,
    payload: &Value,
    approval_bridge_active: bool,
) -> bool {
    approval_bridge_active
        && agent == wmcterm_agent::AgentKind::Codex
        && (event.is_some_and(|name| name.eq_ignore_ascii_case("PermissionRequest"))
            || payload
                .get("hook_event_name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case("PermissionRequest")))
}

fn acknowledge_hook() {
    println!("{{}}");
    let _ = std::io::stdout().flush();
}

fn print_claude_permission_result(output: Value) {
    println!("{output}");
    let _ = std::io::stdout().flush();
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn parsed_env<T: std::str::FromStr>(name: &str) -> Option<T> {
    nonempty_env(name)?.parse().ok()
}

/// Run one delivery on a background thread and cap how long the short-lived
/// hook process waits. Dropping/timing out can only lose status telemetry; it
/// must never block or fail the agent that invoked the hook.
fn bounded_background<T, F>(budget: Duration, task: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("wmcterm-hook-delivery".into())
        .spawn(move || {
            let _ = sender.send(task());
        })
        .ok()?;
    receiver.recv_timeout(budget).ok()
}

fn deliver_hook_bounded(
    envelope: wmcterm_agent::AgentEventEnvelope,
    claude_hook: Option<wmcterm_agent::ClaudeHookEvent>,
) {
    let Ok(mut params) = envelope.to_legacy_rpc_params() else {
        return;
    };
    if let (Some(hook), Some(object)) = (claude_hook, params.as_object_mut())
        && let Ok(hook) = serde_json::to_value(hook)
    {
        object.insert("claude_hook".into(), hook);
    }
    let Ok(path) = control_socket_path() else {
        return;
    };
    let request = Request {
        id: "hook".into(),
        method: "agent.update".into(),
        params,
    };
    let _ = bounded_background(HOOK_DELIVERY_BUDGET, move || notify_ipc(&path, &request));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallScope {
    Project,
    User,
}

impl InstallScope {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            _ => bail!("unsupported scope {value}; expected project or user"),
        }
    }
}

fn agent_config_root(agent: wmcterm_agent::AgentKind, scope: InstallScope) -> Result<PathBuf> {
    let name = match agent {
        wmcterm_agent::AgentKind::Claude => ".claude",
        wmcterm_agent::AgentKind::Codex => ".codex",
        wmcterm_agent::AgentKind::Gemini => ".gemini",
        _ => bail!(
            "automatic hook installation is not available for {agent}; use `wmcterm hook plan --agent {agent}`"
        ),
    };
    match scope {
        InstallScope::Project => Ok(env::current_dir()?.join(name)),
        InstallScope::User => {
            let home = directories::BaseDirs::new()
                .context("could not determine the user home directory")?
                .home_dir()
                .to_path_buf();
            Ok(user_agent_config_root(
                agent,
                &home,
                nonempty_env("CLAUDE_CONFIG_DIR").as_deref().map(Path::new),
            ))
        }
    }
}

fn user_agent_config_root(
    agent: wmcterm_agent::AgentKind,
    home: &Path,
    claude_config_dir: Option<&Path>,
) -> PathBuf {
    if agent == wmcterm_agent::AgentKind::Claude {
        claude_config_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".claude"))
    } else {
        home.join(match agent {
            wmcterm_agent::AgentKind::Codex => ".codex",
            wmcterm_agent::AgentKind::Gemini => ".gemini",
            _ => ".config",
        })
    }
}

fn install_json_fragment(
    path: &Path,
    fragment: Value,
    agent: wmcterm_agent::AgentKind,
) -> Result<usize> {
    let mut document = if path.exists() {
        serde_json::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("invalid existing JSON in {}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let removed = remove_wmcterm_hook_entries(&mut document, agent);
    merge_json_fragment(&mut document, fragment);
    atomic_write_hook_config(
        path,
        format!("{}\n", serde_json::to_string_pretty(&document)?).as_bytes(),
    )?;
    Ok(removed)
}

fn remove_wmcterm_hook_entries(value: &mut Value, agent: wmcterm_agent::AgentKind) -> usize {
    remove_wmcterm_hook_entries_inner(value, agent).removed
}

#[derive(Debug, Default)]
struct HookRemoval {
    removed: usize,
    prune_self: bool,
}

fn remove_wmcterm_hook_entries_inner(
    value: &mut Value,
    agent: wmcterm_agent::AgentKind,
) -> HookRemoval {
    match value {
        Value::Array(items) => {
            let mut removed = 0;
            items.retain_mut(|item| {
                if is_wmcterm_receiver(item, agent) {
                    removed += 1;
                    return false;
                }
                let nested = remove_wmcterm_hook_entries_inner(item, agent);
                removed += nested.removed;
                !nested.prune_self
            });
            HookRemoval {
                removed,
                prune_self: false,
            }
        }
        Value::Object(object) => {
            let mut removed = 0;
            let mut hooks_emptied_by_removal = false;
            for (key, child) in object.iter_mut() {
                let nested = remove_wmcterm_hook_entries_inner(child, agent);
                removed += nested.removed;
                if key == "hooks"
                    && nested.removed > 0
                    && child.as_array().is_some_and(Vec::is_empty)
                {
                    hooks_emptied_by_removal = true;
                }
            }
            HookRemoval {
                removed,
                prune_self: hooks_emptied_by_removal,
            }
        }
        _ => HookRemoval::default(),
    }
}

fn is_wmcterm_receiver(value: &Value, agent: wmcterm_agent::AgentKind) -> bool {
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return false;
    };
    command.contains("wmcterm")
        && command.contains("hook receive")
        && command.contains(&format!("--agent {agent}"))
}

static HOOK_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn atomic_write_hook_config(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let file_name = path
        .file_name()
        .context("hook configuration path has no file name")?
        .to_string_lossy();

    let (temporary_path, mut temporary) = loop {
        let sequence = HOOK_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.wmcterm-{}-{sequence}.tmp",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not create temporary hook config in {}",
                        parent.display()
                    )
                });
            }
        }
    };

    let replacement = (|| -> Result<()> {
        if let Some(permissions) = existing_permissions {
            temporary.set_permissions(permissions)?;
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
        }
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        drop(temporary);
        fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "could not atomically replace hook configuration {}",
                path.display()
            )
        })?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if replacement.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    replacement
}

fn merge_json_fragment(target: &mut Value, fragment: Value) {
    match (target, fragment) {
        (Value::Object(target), Value::Object(fragment)) => {
            for (key, value) in fragment {
                if let Some(existing) = target.get_mut(&key) {
                    merge_json_fragment(existing, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (Value::Array(target), Value::Array(fragment)) => {
            for value in fragment {
                if !target.contains(&value) {
                    target.push(value);
                }
            }
        }
        (target, fragment) => *target = fragment,
    }
}

fn run_skill(mut args: impl Iterator<Item = String>) -> Result<()> {
    let operation = args.next().unwrap_or_else(|| "print".into());
    let mut scope = InstallScope::Project;
    let mut force = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--scope" => {
                scope = InstallScope::parse(&args.next().context("--scope requires a value")?)?
            }
            "--force" => force = true,
            value => bail!("unknown skill option: {value}"),
        }
    }
    match operation.as_str() {
        "print" => print!("{WMCTERM_SKILL}"),
        "install" => {
            let targets = match scope {
                InstallScope::Project => {
                    skill_install_targets(scope, &env::current_dir()?, Path::new(""), None)
                }
                InstallScope::User => {
                    let home = directories::BaseDirs::new()
                        .context("could not determine the user home directory")?
                        .home_dir()
                        .to_path_buf();
                    let claude_config_dir = nonempty_env("CLAUDE_CONFIG_DIR").map(PathBuf::from);
                    skill_install_targets(scope, Path::new(""), &home, claude_config_dir.as_deref())
                }
            };
            for target in targets {
                if target.exists() {
                    let existing = fs::read_to_string(&target)?;
                    if existing == WMCTERM_SKILL {
                        println!("wmcterm skill is already installed at {}", target.display());
                        continue;
                    }
                    if !force && !is_wmcterm_skill(&existing) {
                        bail!(
                            "{} already exists with different content; pass --force to replace it",
                            target.display()
                        );
                    }
                    println!("updating existing wmcterm skill at {}", target.display());
                }
                fs::create_dir_all(target.parent().expect("skill path has a parent"))?;
                fs::write(&target, WMCTERM_SKILL)?;
                println!("installed wmcterm skill at {}", target.display());
            }
        }
        _ => bail!("usage: wmcterm skill print|install [--scope project|user] [--force]"),
    }
    Ok(())
}

fn skill_install_targets(
    scope: InstallScope,
    project: &Path,
    home: &Path,
    claude_config_dir: Option<&Path>,
) -> Vec<PathBuf> {
    match scope {
        InstallScope::Project => vec![
            project.join(".agents/skills/wmcterm/SKILL.md"),
            project.join(".claude/skills/wmcterm/SKILL.md"),
        ],
        InstallScope::User => vec![
            home.join(".agents/skills/wmcterm/SKILL.md"),
            home.join(".codex/skills/wmcterm/SKILL.md"),
            claude_config_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".claude"))
                .join("skills/wmcterm/SKILL.md"),
        ],
    }
}

fn is_wmcterm_skill(contents: &str) -> bool {
    let mut lines = contents.lines();
    lines.next() == Some("---")
        && lines
            .take_while(|line| *line != "---")
            .any(|line| line.trim() == "name: wmcterm")
}

fn run_config(mut args: impl Iterator<Item = String>) -> Result<()> {
    let operation = args.next().unwrap_or_else(|| "show".into());
    let path = wmcterm_config::config_path().context("could not determine wmcterm config path")?;
    match operation.as_str() {
        "path" => println!("{}", path.display()),
        "show" => println!(
            "{}",
            toml::to_string_pretty(&wmcterm_config::load_or_default()?)?
        ),
        "validate" => {
            let config = wmcterm_config::load_or_default()?;
            config.theme.validate()?;
            config.resolved_keybindings()?;
            println!("configuration is valid: {}", path.display());
        }
        "init" => {
            if path.exists() {
                bail!("configuration already exists: {}", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = toml::to_string_pretty(&wmcterm_config::Config::default())?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            use std::io::Write;
            file.write_all(content.as_bytes())?;
            println!("created {}", path.display());
        }
        "import-ghostty" => {
            let source = args
                .next()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    directories::BaseDirs::new()
                        .map(|dirs| dirs.config_dir().join("ghostty/config"))
                        .unwrap_or_default()
                });
            let mut config = wmcterm_config::load_or_default()?;
            let imported = wmcterm_config::load_ghostty(&source, &[])?;
            config.apply_ghostty(&imported);
            println!("{}", toml::to_string_pretty(&config)?);
            for warning in imported.warnings {
                eprintln!("warning: {warning}");
            }
        }
        _ => bail!("usage: wmcterm config path|show|validate|init|import-ghostty [PATH]"),
    }
    Ok(())
}

fn shell_arg(value: &str) -> String {
    #[cfg(windows)]
    {
        format!("'{}'", value.replace('\'', "''"))
    }
    #[cfg(not(windows))]
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn parse_args() -> Result<(TerminalSize, CommandSpec)> {
    let mut args = env::args().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "dump") {
        args.next();
    }
    let mut cols = 80_u16;
    let mut rows = 24_u16;

    while let Some(flag) = args.peek().map(String::as_str) {
        match flag {
            "--cols" => {
                args.next();
                cols = args
                    .next()
                    .context("--cols requires a value")?
                    .parse()
                    .context("invalid --cols value")?;
            }
            "--rows" => {
                args.next();
                rows = args
                    .next()
                    .context("--rows requires a value")?
                    .parse()
                    .context("invalid --rows value")?;
            }
            "--" => {
                args.next();
                break;
            }
            value if value.starts_with('-') => bail!("unknown option: {value}"),
            _ => break,
        }
    }

    let command: Vec<_> = args.collect();
    let command = if command.is_empty() {
        default_dump_command()
    } else {
        CommandSpec::new(&command[0]).args(command.iter().skip(1))
    };

    Ok((TerminalSize::new(cols, rows)?, command))
}

fn default_dump_command() -> CommandSpec {
    #[cfg(windows)]
    {
        CommandSpec::new("powershell.exe").args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Write-Output 'wmcterm phase 0'",
        ])
    }
    #[cfg(not(windows))]
    {
        CommandSpec::new("sh").args(["-lc", "printf 'wmcterm phase 0\\n'"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wmcterm_agent::AgentKind;

    #[test]
    fn terminal_wait_durations_are_bounded_and_human_readable() {
        assert_eq!(
            parse_cli_duration("250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(parse_cli_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_cli_duration("3").unwrap(), Duration::from_secs(3));
        assert!(parse_cli_duration("0ms").is_err());
        assert!(parse_cli_duration("301s").is_err());
    }

    #[test]
    fn screen_wait_rejects_multiple_conditions() {
        let mut condition = None;
        set_wait_condition(&mut condition, ScreenWaitCondition::Change).unwrap();
        assert!(
            set_wait_condition(&mut condition, ScreenWaitCondition::Until("prompt".into()))
                .is_err()
        );
    }

    #[test]
    fn windows_version_floor_includes_windows_11() {
        assert!(!windows_version_supported(10, 17_762));
        assert!(windows_version_supported(10, 17_763));
        assert!(windows_version_supported(10, 22_000));
        assert!(windows_version_supported(11, 0));
    }

    #[test]
    fn windows_codex_shim_resolves_native_optional_dependency_layouts() {
        let candidates = codex_native_candidates(Path::new("C:/Users/user/npm/codex.cmd"));
        assert_eq!(candidates.len(), 3);
        assert!(candidates.iter().all(|path| path.ends_with("codex.exe")));
        assert!(
            candidates[0]
                .to_string_lossy()
                .contains("@openai/codex-win32-x64")
        );
    }

    #[test]
    fn agent_control_keys_encode_terminal_bytes() {
        assert_eq!(encode_control_key("enter").unwrap().as_bytes(), b"\r");
        assert_eq!(encode_control_key("ctrl+c").unwrap().as_bytes(), &[3]);
        assert_eq!(encode_control_key("Ctrl+D").unwrap().as_bytes(), &[4]);
        assert_eq!(encode_control_key("^Z").unwrap().as_bytes(), &[26]);
        assert_eq!(encode_control_key("up").unwrap().as_bytes(), b"\x1b[A");
        assert!(encode_control_key("ctrl+1").is_err());
    }

    #[test]
    fn claude_permission_detection_uses_explicit_or_payload_event_name() {
        assert!(claude_permission_event(
            Some("PermissionRequest"),
            &serde_json::json!({})
        ));
        assert!(claude_permission_event(
            Some("Stop"),
            &serde_json::json!({"hook_event_name":"PermissionRequest"})
        ));
        assert!(!claude_permission_event(
            Some("Stop"),
            &serde_json::json!({"hook_event_name":"Stop"})
        ));
    }

    #[test]
    fn managed_codex_bridge_suppresses_only_duplicate_permission_hooks() {
        let payload = serde_json::json!({"hook_event_name":"PermissionRequest"});
        assert!(suppress_bridged_codex_permission_hook(
            AgentKind::Codex,
            None,
            &payload,
            true,
        ));
        assert!(suppress_bridged_codex_permission_hook(
            AgentKind::Codex,
            Some("PermissionRequest"),
            &serde_json::json!({}),
            true,
        ));
        assert!(!suppress_bridged_codex_permission_hook(
            AgentKind::Codex,
            None,
            &payload,
            false,
        ));
        assert!(!suppress_bridged_codex_permission_hook(
            AgentKind::Claude,
            None,
            &payload,
            true,
        ));
        assert!(!suppress_bridged_codex_permission_hook(
            AgentKind::Codex,
            Some("Stop"),
            &serde_json::json!({"hook_event_name":"Stop"}),
            true,
        ));
    }

    #[test]
    fn cli_environment_assignment_preserves_value_equals() {
        assert_eq!(
            parse_environment_assignment("TOKEN=a=b").unwrap(),
            ("TOKEN".into(), "a=b".into())
        );
        assert!(parse_environment_assignment("MISSING").is_err());
        assert!(parse_environment_assignment("=value").is_err());
    }

    #[test]
    fn managed_agent_runs_in_the_calling_terminal_without_surface_rpc() {
        let (mode, program, supplied) = parse_agent_launch_args(
            &mut ["--mode".into(), "unattended".into(), "codex".into()].into_iter(),
        )
        .unwrap();
        assert_eq!(mode, AgentRunMode::Unattended);
        assert_eq!(program, "codex");
        assert_eq!(supplied, Vec::<String>::new());

        let (program, args) = plan_agent_process(
            AgentRunMode::Interactive,
            "claude",
            &["--model".into(), "sonnet".into()],
        )
        .unwrap();
        assert_eq!(program, PathBuf::from("claude"));
        assert_eq!(args, ["--model", "sonnet"]);

        let (program, args) = plan_agent_process(AgentRunMode::Interactive, "codex", &[]).unwrap();
        assert_eq!(program, env::current_exe().unwrap());
        assert_eq!(args[0], "codex-teams");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--ask-for-approval", "on-request"])
        );
    }

    #[test]
    fn close_others_anchor_is_explicit_or_injected_by_cli_context() {
        let mut explicit = vec!["--anchor".to_owned(), "pane-a".to_owned()].into_iter();
        assert_eq!(
            parse_anchor(&mut explicit, Some("pane-env".into()), "pane").unwrap(),
            "pane-a"
        );
        assert_eq!(
            parse_anchor(&mut std::iter::empty(), Some("pane-env".into()), "pane").unwrap(),
            "pane-env"
        );
        assert!(parse_anchor(&mut std::iter::empty(), None, "pane").is_err());
    }

    #[test]
    fn hook_fragments_preserve_settings_append_events_and_deduplicate() {
        let existing_hook = serde_json::json!({"hooks": [{"type": "command", "command": "old"}]});
        let mut target = serde_json::json!({
            "theme": "dark",
            "hooks": {"Stop": [existing_hook.clone()]}
        });
        merge_json_fragment(
            &mut target,
            serde_json::json!({
                "hooks": {
                    "Stop": [existing_hook, {"hooks": [{"type": "command", "command": "wmcterm"}]}],
                    "SessionStart": [{"hooks": [{"type": "command", "command": "wmcterm"}]}]
                }
            }),
        );

        assert_eq!(target["theme"], "dark");
        assert_eq!(target["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(target["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_upgrade_removes_only_matching_stale_wmcterm_receivers() {
        let mut document = serde_json::json!({
            "theme": "dark",
            "hooks": {
                "Stop": [
                    {"hooks": [{"type": "command", "command": "/old/wmcterm hook receive --agent codex"}]},
                    {"hooks": [{"type": "command", "command": "keep-me"}]},
                    {"hooks": [{"type": "command", "command": "/old/wmcterm hook receive --agent claude"}]},
                    {"hooks": []},
                    {"hooks": [], "disabled": true}
                ]
            }
        });
        assert_eq!(
            remove_wmcterm_hook_entries(&mut document, wmcterm_agent::AgentKind::Codex),
            1
        );
        let stop = document["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 4);
        assert!(
            stop.iter()
                .any(|entry| entry.to_string().contains("keep-me"))
        );
        assert!(
            stop.iter()
                .any(|entry| entry.to_string().contains("--agent claude"))
        );
        assert!(
            stop.iter()
                .any(|entry| entry == &serde_json::json!({"hooks": []}))
        );
        assert!(
            stop.iter()
                .any(|entry| entry == &serde_json::json!({"hooks": [], "disabled": true}))
        );
        assert_eq!(document["theme"], "dark");
    }

    #[test]
    fn hook_install_atomically_replaces_config_and_preserves_permissions() {
        let directory = env::temp_dir().join(format!(
            "wmcterm-hook-atomic-{}-{}",
            std::process::id(),
            HOOK_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("hooks.json");
        fs::write(&path, "{\"theme\":\"keep\",\"hooks\":{}}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }

        let removed = install_json_fragment(
            &path,
            serde_json::json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/wmcterm hook receive --agent codex"}]}]
                }
            }),
            wmcterm_agent::AgentKind::Codex,
        )
        .unwrap();

        assert_eq!(removed, 0);
        let installed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(installed["theme"], "keep");
        assert!(installed["hooks"]["Stop"].is_array());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")
        }));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let new_path = directory.join("new-hooks.json");
            atomic_write_hook_config(&new_path, b"{}\n").unwrap();
            assert_eq!(
                fs::metadata(new_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bundled_skill_has_required_metadata_and_control_context() {
        assert!(WMCTERM_SKILL.starts_with("---\nname: wmcterm\n"));
        assert!(WMCTERM_SKILL.contains("description:"));
        assert!(WMCTERM_SKILL.contains("WMCTERM_CONTROL_SOCKET"));
        assert!(WMCTERM_SKILL.contains("wmcterm capabilities"));
        assert!(is_wmcterm_skill(WMCTERM_SKILL));
        assert!(!is_wmcterm_skill("---\nname: someone-else\n---\n"));
    }

    #[test]
    fn user_skill_install_covers_shared_codex_and_claude_roots() {
        assert_eq!(
            skill_install_targets(
                InstallScope::User,
                Path::new("/project"),
                Path::new("/home/user"),
                None,
            ),
            vec![
                PathBuf::from("/home/user/.agents/skills/wmcterm/SKILL.md"),
                PathBuf::from("/home/user/.codex/skills/wmcterm/SKILL.md"),
                PathBuf::from("/home/user/.claude/skills/wmcterm/SKILL.md"),
            ]
        );
        assert_eq!(
            skill_install_targets(
                InstallScope::Project,
                Path::new("/project"),
                Path::new("/home/user"),
                None,
            ),
            vec![
                PathBuf::from("/project/.agents/skills/wmcterm/SKILL.md"),
                PathBuf::from("/project/.claude/skills/wmcterm/SKILL.md"),
            ]
        );
    }

    #[test]
    fn claude_config_override_controls_user_hook_and_skill_roots() {
        let home = Path::new("/home/user");
        let claude = Path::new("/custom/claude");
        assert_eq!(
            user_agent_config_root(wmcterm_agent::AgentKind::Claude, home, Some(claude)),
            claude
        );
        assert_eq!(
            user_agent_config_root(wmcterm_agent::AgentKind::Codex, home, Some(claude)),
            PathBuf::from("/home/user/.codex")
        );
        assert_eq!(
            skill_install_targets(InstallScope::User, Path::new(""), home, Some(claude)),
            vec![
                PathBuf::from("/home/user/.agents/skills/wmcterm/SKILL.md"),
                PathBuf::from("/home/user/.codex/skills/wmcterm/SKILL.md"),
                PathBuf::from("/custom/claude/skills/wmcterm/SKILL.md"),
            ]
        );
    }

    #[test]
    fn bounded_hook_delivery_returns_fast_results_and_times_out_slow_ones() {
        assert_eq!(bounded_background(Duration::from_secs(1), || 42), Some(42));
        assert_eq!(
            bounded_background(Duration::ZERO, || {
                thread::sleep(Duration::from_millis(20));
                7
            }),
            None
        );
    }

    #[test]
    fn canonical_agent_envelope_adapts_to_existing_rpc_shape() {
        let envelope = wmcterm_agent::AgentEventEnvelope {
            surface_id: Some("surface-a".into()),
            lifecycle_id: Some("life-a".into()),
            agent_pid: Some(123),
            pid_start_time: Some(456),
            version: Some(1),
            sequence: Some(8),
            event: wmcterm_agent::AgentEvent::ToolUse {
                tool: "cargo".into(),
            },
        };
        let canonical = serde_json::to_value(&envelope).unwrap();
        let params = wmcterm_agent::AgentEventEnvelope::from_compatible_value(canonical)
            .unwrap()
            .to_legacy_rpc_params()
            .unwrap();
        assert_eq!(params["event"], "tool_use");
        assert_eq!(params["surface"], "surface-a");
        assert_eq!(params["sequence"], 8);
    }

    #[test]
    fn codex_teams_configuration_is_pure_and_caps_depth() {
        let config = codex_teams_config(
            Some("workspace-a".into()),
            Some("surface-a".into()),
            Some("/tmp/wmcterm.sock".into()),
            Some("999"),
            Some("/opt/codex test/bin/codex".into()),
            "ws://127.0.0.1:40000".into(),
        )
        .unwrap();
        assert_eq!(config.workspace_id, "workspace-a");
        assert_eq!(config.root_surface_id, "surface-a");
        assert_eq!(config.control_socket, PathBuf::from("/tmp/wmcterm.sock"));
        assert_eq!(
            config.codex_program,
            PathBuf::from("/opt/codex test/bin/codex")
        );
        assert_eq!(
            config.max_auto_depth,
            wmcterm_codex::ABSOLUTE_MAX_AUTO_DEPTH
        );
    }

    #[test]
    fn codex_teams_configuration_rejects_missing_terminal_context() {
        let missing_workspace = codex_teams_config(
            None,
            Some("surface".into()),
            Some("socket".into()),
            None,
            None,
            "ws://127.0.0.1:1".into(),
        )
        .unwrap_err();
        assert!(
            missing_workspace
                .to_string()
                .contains("inside a wmcterm terminal")
        );

        let missing_socket = codex_teams_config(
            Some("workspace".into()),
            Some("surface".into()),
            None,
            Some("invalid"),
            None,
            "ws://127.0.0.1:1".into(),
        )
        .unwrap_err();
        assert!(missing_socket.to_string().contains("socket is unavailable"));
    }

    #[test]
    fn codex_launch_defaults_to_on_request_approvals_and_workspace_sandbox() {
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Interactive,
                &["--model".into(), "gpt-test".into()]
            )
            .unwrap(),
            [
                "--ask-for-approval",
                "on-request",
                "--sandbox",
                "workspace-write",
                "--model",
                "gpt-test"
            ]
        );
    }

    #[test]
    fn codex_launch_preserves_explicit_permission_dimensions() {
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Interactive,
                &["-a".into(), "on-request".into()]
            )
            .unwrap(),
            ["--sandbox", "workspace-write", "-a", "on-request"]
        );
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Interactive,
                &["-s".into(), "read-only".into()]
            )
            .unwrap(),
            ["--ask-for-approval", "on-request", "-s", "read-only"]
        );
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Interactive,
                &[
                    "--ask-for-approval=on-request".into(),
                    "-s".into(),
                    "read-only".into(),
                ],
            )
            .unwrap(),
            ["--ask-for-approval=on-request", "-s", "read-only"]
        );
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Interactive,
                &[
                    "-c".into(),
                    "approval_policy=untrusted".into(),
                    "--config=sandbox_mode=read-only".into(),
                ],
            )
            .unwrap(),
            [
                "-c",
                "approval_policy=untrusted",
                "--config=sandbox_mode=read-only"
            ]
        );
    }

    #[test]
    fn codex_launch_rejects_fully_unsafe_shortcuts() {
        for option in [
            "--yolo",
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-approvals-and-sandbox=true",
        ] {
            assert!(plan_codex_launch_args(AgentRunMode::Interactive, &[option.into()]).is_err());
        }
        assert!(
            plan_codex_launch_args(AgentRunMode::Interactive, &["--".into(), "--yolo".into()])
                .is_ok()
        );
    }

    #[test]
    fn codex_launch_unattended_injects_approve_for_me_profile() {
        assert_eq!(
            plan_codex_launch_args(
                AgentRunMode::Unattended,
                &["--model".into(), "gpt-test".into()]
            )
            .unwrap(),
            ["--approve-for-me", "--model", "gpt-test"]
        );
    }

    #[test]
    fn codex_unattended_rejects_an_explicit_sandbox_without_an_approval_policy() {
        assert!(
            plan_codex_launch_args(
                AgentRunMode::Unattended,
                &["--sandbox".into(), "workspace-write".into()]
            )
            .is_err()
        );
        assert_eq!(
            plan_codex_launch_args(AgentRunMode::Unattended, &["--approve-for-me".into()]).unwrap(),
            ["--approve-for-me"]
        );
    }

    #[test]
    fn codex_launch_rejects_cli_full_access_mode_without_ui_authorization() {
        assert!(plan_codex_launch_args(AgentRunMode::FullAccess, &["--model".into()]).is_err());
    }

    #[test]
    fn claude_unattended_uses_native_auto_mode_and_rejects_bypass() {
        assert_eq!(
            plan_claude_launch_args(
                AgentRunMode::Unattended,
                &["--model".into(), "sonnet".into()]
            )
            .unwrap(),
            ["--permission-mode", "auto", "--model", "sonnet"]
        );
        assert_eq!(
            plan_claude_launch_args(
                AgentRunMode::Unattended,
                &["--permission-mode=manual".into()]
            )
            .unwrap(),
            ["--permission-mode=manual"]
        );
        for option in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--permission-mode=bypassPermissions",
        ] {
            assert!(plan_claude_launch_args(AgentRunMode::Interactive, &[option.into()]).is_err());
        }
    }

    #[test]
    fn managed_codex_launcher_rejects_unsandboxed_variants() {
        for args in [
            vec!["--sandbox=danger-full-access".into()],
            vec!["-s".into(), "danger-full-access".into()],
            vec!["-c".into(), "sandbox_mode=danger-full-access".into()],
            vec!["--config=sandbox_mode='danger-full-access'".into()],
        ] {
            assert!(plan_codex_launch_args(AgentRunMode::Interactive, &args).is_err());
        }
    }

    #[test]
    fn noninteractive_modes_are_limited_to_supported_managed_agents() {
        assert!(plan_agent_process(AgentRunMode::Unattended, "gemini", &[]).is_err());
        assert!(plan_agent_process(AgentRunMode::FullAccess, "codex", &[]).is_err());
        assert!(plan_agent_process(AgentRunMode::FullAccess, "claude", &[]).is_err());
    }

    #[test]
    fn parse_agent_launch_mode_preserves_legacy_and_rejects_invalid_mode() {
        let (mode, program, supplied) = parse_agent_launch_args(
            &mut ["claude".into(), "--model".into(), "sonnet".into()].into_iter(),
        )
        .unwrap();
        assert_eq!(mode, AgentRunMode::Interactive);
        assert_eq!(program, "claude");
        assert_eq!(supplied, ["--model", "sonnet"]);

        let (mode, program, supplied) = parse_agent_launch_args(
            &mut [
                "--mode=unattended".into(),
                "codex".into(),
                "--model".into(),
                "gpt-test".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(mode, AgentRunMode::Unattended);
        assert_eq!(program, "codex");
        assert_eq!(supplied, ["--model", "gpt-test"]);

        assert!(
            parse_agent_launch_args(
                &mut ["--mode".into(), "invalid".into(), "codex".into()].into_iter()
            )
            .is_err()
        );
    }
}
