//! Always-on session diagnostics for tracking down mystery disconnects.
//!
//! Timestamped, append-only lines at `~/.xxssh/session-debug.log` (UTC).
//! Low-frequency events only: connect milestones, transport-level closes
//! (russh Handler::disconnected), session-end forensics (uptime/idle).

use std::io::Write;

pub fn log(msg: &str) {
    let Ok(home) = dirs::home_dir().ok_or(()) else { return };
    let path = home.join(".xxssh").join("session-debug.log");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ts = crate::sftp::fmt_time_unix(now.as_secs());
    let line = format!("{}.{:03} {}\n", ts.trim_end_matches(" UTC"), now.subsec_millis(), msg);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}
