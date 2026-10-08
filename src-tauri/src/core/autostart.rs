/// Register a per-user elevated logon task; no shell interpolation or stored password.
#[tauri::command]
pub async fn configure_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        tauri::async_runtime::spawn_blocking(move || configure(enabled))
            .await
            .map_err(|error| error.to_string())?
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("This command is only available on Windows".into())
    }
}

#[cfg(windows)]
fn configure(enabled: bool) -> Result<(), String> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let system_root = std::env::var_os("SystemRoot").ok_or("SystemRoot is missing")?;
    let powershell = std::path::PathBuf::from(system_root)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(include_str!("autostart.ps1"))
        .env("BONGO_AUTOSTART_EXE", exe)
        .env("BONGO_AUTOSTART_ENABLED", if enabled { "1" } else { "0" })
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            return if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            };
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Configuring Windows startup timed out; please try again".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
