use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(any(unix, windows))]
use std::time::Instant;

use portable_pty::{CommandBuilder, PtySize as NativePtySize, native_pty_system};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<std::path::PathBuf>,
    env: Vec<(OsString, OsString)>,
}

impl CommandSpec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }
}

#[derive(Debug)]
pub struct PtyOutput {
    pub bytes: Vec<u8>,
    pub status: portable_pty::ExitStatus,
}

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("PTY operation failed: {0}")]
    Operation(#[from] anyhow::Error),
    #[error("failed reading PTY: {0}")]
    Read(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub enum PtyCommand {
    Input(Vec<u8>),
    Resize(PtySize),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum PtyEvent {
    Output(Vec<u8>),
    /// Command lines for the foreground process group followed by its parent
    /// chain. Empty means process metadata is unavailable.
    ForegroundProcess(Vec<String>),
    /// Live descendants of the shell process. The shell itself is excluded,
    /// so an empty list represents an idle terminal.
    ChildProcesses(Vec<String>),
    Exited(portable_pty::ExitStatus),
    Error(String),
}

/// Handle to an interactive PTY actor. Dropping it requests shutdown.
pub struct PtySession {
    commands: flume::Sender<PtyCommand>,
    events: flume::Receiver<PtyEvent>,
    worker: Option<JoinHandle<()>>,
}

const MAX_PENDING_PROTOCOL_REPLIES: usize = 8;

impl PtySession {
    pub fn spawn(command: CommandSpec, size: PtySize) -> Result<Self, PtyError> {
        let pair = native_pty_system().openpty(size.into())?;
        let mut builder = CommandBuilder::new(&command.program);
        builder.args(&command.args);
        if let Some(cwd) = &command.cwd {
            builder.cwd(cwd);
        }
        for (key, value) in &command.env {
            builder.env(key, value);
        }
        let child = pair.slave.spawn_command(builder)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (command_tx, command_rx) = flume::unbounded();
        let (event_tx, event_rx) = flume::unbounded();
        let worker = thread::Builder::new()
            .name("wmcterm-pty".into())
            .spawn(move || run_actor(pair.master, child, reader, writer, command_rx, event_tx))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            worker: Some(worker),
        })
    }

    pub fn send(&self, command: PtyCommand) -> Result<(), flume::SendError<PtyCommand>> {
        self.commands.send(command)
    }

    /// Queue a terminal-generated protocol reply without allowing hostile
    /// output to grow the PTY command queue without bound. Interactive input
    /// continues to use [`Self::send`] and is never dropped by this guard.
    pub fn send_protocol_reply(
        &self,
        bytes: Vec<u8>,
    ) -> Result<bool, flume::SendError<PtyCommand>> {
        if bytes.is_empty() {
            return Ok(true);
        }
        if self.commands.len() >= MAX_PENDING_PROTOCOL_REPLIES {
            return Ok(false);
        }
        self.commands.send(PtyCommand::Input(bytes)).map(|()| true)
    }

    pub fn events(&self) -> &flume::Receiver<PtyEvent> {
        &self.events
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.commands.send(PtyCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_actor(
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
    commands: flume::Receiver<PtyCommand>,
    events: flume::Sender<PtyEvent>,
) {
    #[cfg(any(unix, windows))]
    let root_process_id = child.process_id();
    let (output_tx, output_rx) = flume::unbounded();
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = output_tx.send(format!("PTY read failed: {error}").into_bytes());
                    break;
                }
            }
        }
    });
    // A PTY write can block indefinitely when the foreground program stops
    // consuming input (a large paste is the common case). Keep that blocking
    // syscall off the lifecycle actor so Shutdown can still kill the child and
    // let TerminalSession::drop return without freezing the UI thread.
    let (input_tx, input_rx) = flume::unbounded::<Vec<u8>>();
    let writer_events = events.clone();
    thread::spawn(move || {
        while let Ok(bytes) = input_rx.recv() {
            if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                let _ = writer_events.send(PtyEvent::Error(error.to_string()));
                return;
            }
        }
    });

    #[cfg(any(unix, windows))]
    let mut next_process_check = Instant::now();
    #[cfg(any(unix, windows))]
    let mut last_foreground = Vec::new();
    #[cfg(any(target_os = "linux", windows))]
    let mut last_children = Vec::new();

    loop {
        while let Ok(bytes) = output_rx.try_recv() {
            if events.send(PtyEvent::Output(bytes)).is_err() {
                let _ = child.kill();
                return;
            }
        }
        #[cfg(any(unix, windows))]
        if Instant::now() >= next_process_check {
            next_process_check = Instant::now() + Duration::from_millis(500);
            #[cfg(unix)]
            let foreground = master
                .process_group_leader()
                .and_then(|pid| foreground_process_chain(pid, root_process_id))
                .unwrap_or_default();
            #[cfg(windows)]
            let foreground = root_process_id
                .map(descendant_processes)
                .unwrap_or_default();
            if foreground != last_foreground {
                last_foreground.clone_from(&foreground);
                if events
                    .send(PtyEvent::ForegroundProcess(foreground))
                    .is_err()
                {
                    let _ = child.kill();
                    return;
                }
            }
            #[cfg(any(target_os = "linux", windows))]
            {
                let children = root_process_id
                    .map(descendant_processes)
                    .unwrap_or_default();
                if children != last_children {
                    last_children.clone_from(&children);
                    if events.send(PtyEvent::ChildProcesses(children)).is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                while let Ok(bytes) = output_rx.recv_timeout(Duration::from_millis(10)) {
                    let _ = events.send(PtyEvent::Output(bytes));
                }
                let _ = events.send(PtyEvent::Exited(status));
                return;
            }
            Err(error) => {
                let _ = events.send(PtyEvent::Error(error.to_string()));
                return;
            }
            Ok(None) => {}
        }
        match commands.recv_timeout(Duration::from_millis(8)) {
            Ok(PtyCommand::Input(bytes)) => {
                if input_tx.send(bytes).is_err() {
                    let _ = events.send(PtyEvent::Error("PTY writer stopped".into()));
                    return;
                }
            }
            Ok(PtyCommand::Resize(size)) => {
                if let Err(error) = master.resize(size.into()) {
                    let _ = events.send(PtyEvent::Error(error.to_string()));
                }
            }
            Ok(PtyCommand::Shutdown) | Err(flume::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(flume::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn foreground_process_chain(pid: i32, stop_at: Option<u32>) -> Option<Vec<String>> {
    let mut commands = Vec::new();
    let mut current = u32::try_from(pid).ok()?;
    for _ in 0..24 {
        if current <= 1 {
            break;
        }
        let root = std::path::PathBuf::from(format!("/proc/{current}"));
        let command_line = std::fs::read(root.join("cmdline"))
            .ok()
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .split('\0')
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|line| !line.is_empty())
            .or_else(|| {
                std::fs::read_to_string(root.join("comm"))
                    .ok()
                    .map(|line| line.trim().to_owned())
            });
        if let Some(command_line) = command_line {
            commands.push(command_line);
        }
        if stop_at == Some(current) {
            break;
        }
        let status = std::fs::read_to_string(root.join("status")).ok()?;
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))?
            .trim()
            .parse::<u32>()
            .ok()?;
        if parent == current {
            break;
        }
        current = parent;
    }
    Some(commands)
}

#[cfg(target_os = "linux")]
fn descendant_processes(root: u32) -> Vec<String> {
    let mut pending = vec![root];
    let mut commands = Vec::new();
    while let Some(parent) = pending.pop() {
        let children_path = format!("/proc/{parent}/task/{parent}/children");
        let Ok(children) = std::fs::read_to_string(children_path) else {
            continue;
        };
        for child in children
            .split_ascii_whitespace()
            .filter_map(|value| value.parse::<u32>().ok())
        {
            pending.push(child);
            let root = std::path::PathBuf::from(format!("/proc/{child}"));
            let command = std::fs::read(root.join("cmdline"))
                .ok()
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .split('\0')
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|line| !line.is_empty())
                .or_else(|| {
                    std::fs::read_to_string(root.join("comm"))
                        .ok()
                        .map(|line| line.trim().to_owned())
                });
            if let Some(command) = command {
                commands.push(command);
            }
        }
    }
    commands.sort();
    commands
}

#[cfg(windows)]
fn descendant_processes(root: u32) -> Vec<String> {
    use std::collections::HashSet;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut processes = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut available = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while available {
        let name_end = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.szExeFile.len());
        processes.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..name_end]),
        ));
        available = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }

    let mut descendants = HashSet::from([root]);
    loop {
        let previous_len = descendants.len();
        for (process_id, parent_id, _) in &processes {
            if descendants.contains(parent_id) {
                descendants.insert(*process_id);
            }
        }
        if descendants.len() == previous_len {
            break;
        }
    }
    let mut commands = processes
        .into_iter()
        .filter(|(process_id, _, _)| *process_id != root && descendants.contains(process_id))
        .map(|(_, _, name)| name)
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

#[cfg(all(unix, not(target_os = "linux")))]
fn foreground_process_chain(_pid: i32, _stop_at: Option<u32>) -> Option<Vec<String>> {
    None
}

impl From<PtySize> for NativePtySize {
    fn from(size: PtySize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

pub fn run_to_end(command: &CommandSpec, size: PtySize) -> Result<PtyOutput, PtyError> {
    let pair = native_pty_system().openpty(size.into())?;
    let mut builder = CommandBuilder::new(&command.program);
    builder.args(&command.args);
    if let Some(cwd) = &command.cwd {
        builder.cwd(cwd);
    }
    for (key, value) in &command.env {
        builder.env(key, value);
    }
    let mut child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    let status = child.wait()?;
    Ok(PtyOutput { bytes, status })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn captures_output_from_a_real_pty() {
        let command = CommandSpec::new("sh").args(["-lc", "printf '\\033[32mhello\\033[0m\\n'"]);
        let output = run_to_end(&command, PtySize::new(80, 24)).unwrap();
        assert!(output.status.success());
        assert!(output.bytes.windows(5).any(|window| window == b"hello"));
        assert!(output.bytes.contains(&0x1b));
    }

    #[test]
    fn interactive_session_accepts_input() {
        let session = PtySession::spawn(
            CommandSpec::new("sh").args(["-lc", "read line; printf 'got:%s\\n' \"$line\""]),
            PtySize::new(80, 24),
        )
        .unwrap();
        session.send(PtyCommand::Input(b"ping\n".to_vec())).unwrap();
        let mut output = Vec::new();
        loop {
            match session
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
            {
                PtyEvent::Output(bytes) => output.extend(bytes),
                PtyEvent::ForegroundProcess(_) => {}
                PtyEvent::ChildProcesses(_) => {}
                PtyEvent::Exited(status) => {
                    assert!(status.success());
                    break;
                }
                PtyEvent::Error(error) => panic!("{error}"),
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("got:ping"));
    }

    #[test]
    fn shutdown_does_not_wait_for_a_blocked_large_input_write() {
        let session = PtySession::spawn(
            CommandSpec::new("sh").args(["-lc", "sleep 10"]),
            PtySize::new(80, 24),
        )
        .unwrap();
        session
            .send(PtyCommand::Input(vec![b'x'; 8 * 1024 * 1024]))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let started = Instant::now();
        drop(session);

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "PTY shutdown waited for blocked input: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn discovers_live_descendants_without_counting_the_shell_itself() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 2"])
            .spawn()
            .unwrap();
        let mut descendants = Vec::new();
        for _ in 0..20 {
            descendants = descendant_processes(child.id());
            if !descendants.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(descendants.iter().any(|command| command.contains("sleep")));
    }
}
