#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let executable = std::env::current_exe()?.with_file_name("wmcterm.exe");
    std::process::Command::new(executable)
        .arg("gui")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("wmcterm-gui is only used by the Windows installer");
}
