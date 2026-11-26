//! 64-bit host logging. The host is a GUI-subsystem process, so all diagnostics
//! go to `ddraw-host.log` next to the executable (or `CNC_DDRAW_HOST_LOG`).
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CNC_DDRAW_HOST_LOG") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|dir| dir.join("ddraw-host.log"))
}

pub fn init() {
    LOG.get_or_init(|| {
        let path = log_path()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
}

pub fn log_msg(message: &str) {
    let Some(file) = LOG.get().and_then(Option::as_ref) else {
        return;
    };
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "[ddraw-host] {message}");
        let _ = file.flush();
    }
}
