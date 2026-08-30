//! Local shell terminals (CMD / PowerShell / $SHELL) through a real PTY.
//!
//! Adapts a portable-pty ConPTY (Windows) / openpty (unix) process to the same
//! `SessionHandle` channel shape the SSH sessions use, so the terminal widget,
//! tab bar and resize/close plumbing work unchanged.
//!
//! Colors and completion (clink for cmd, PSReadLine for PowerShell) work because
//! the child runs on a real PTY; clink's cmd AutoRun injection applies as usual.

use tokio::sync::mpsc;

use crate::session::{SessionEvent, SessionHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Cmd,
    PowerShell,
    DefaultShell,
}

/// Spawn a local shell on a new PTY; returns the same handle shape as SSH tabs.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    kind: ShellKind,
    cols: u16,
    rows: u16,
) -> Result<SessionHandle, String> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: rows.max(2) as u16,
            cols: cols.max(2) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut cmd = if let Ok(force) = std::env::var("XXSSHG_LOCAL_CMD") {
        let mut c = portable_pty::CommandBuilder::new(force);
        if let Some(args) = std::env::var("XXSSHG_LOCAL_ARGS").ok() {
            c.arg(args);
        }
        c
    } else {
        match kind {
        ShellKind::Cmd => portable_pty::CommandBuilder::new("cmd.exe"),
        ShellKind::PowerShell => portable_pty::CommandBuilder::new("powershell.exe"),
            ShellKind::DefaultShell => portable_pty::CommandBuilder::new(
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
            ),
        }
    };
    cmd.cwd(std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into()));

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| format!("spawn: {e}"))?;
    let mut reader = pair.master.try_clone_reader().map_err(|e| format!("reader: {e}"))?;
    let mut writer = pair.master.take_writer().map_err(|e| format!("writer: {e}"))?;
    // ConPTY enables win32-input-mode (ESC[?9001h) and then expects KEYSTROKE
    // ENCODING sequences on the input pipe, silently dropping plain bytes.
    // We're a plain-byte terminal: turn the mode off immediately.
    if cfg!(windows) {
        let _ = writer.write_all(b"[?9001l");
        let _ = writer.flush();
    }

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();

    // PTY -> GUI output (blocking reads on a plain thread)
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[diag] pty reader: EOF");
                    break;
                }
                Ok(n) => {
                    if output_tx.send(buf[..n].to_vec()).is_err() {
                        eprintln!("[diag] pty reader: output channel closed");
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("[diag] pty reader: ERR {e}");
                    break;
                }
            }
        }
    });

    // GUI input + resize (async channel ends, driven on the shared runtime)
    let pty = pair.master;
    rt.spawn(async move {
        loop {
            tokio::select! {
                data = input_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            eprintln!("[diag] input task: {} bytes -> pty", bytes.len());
                            let w = writer.write_all(&bytes);
                            eprintln!("[diag] write result: {:?}", w.as_ref().map(|_| ()));
                            let _ = w.map_err(|e| eprintln!("[diag] write ERR: {e}"));
                            let _ = writer.flush();
                        }
                        None => { eprintln!("[diag] input channel closed"); break; }
                    }
                }
                size = resize_rx.recv() => {
                    match size {
                        Some((cols, rows)) => {
                            let _ = pty.resize(portable_pty::PtySize {
                                rows: rows.max(2),
                                cols: cols.max(2),
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                        None => break,
                    }
                }
            }
        }
    });

    // Watch for exit or a close request from the UI.
    // NB: the slave handle MUST be kept alive for the whole session — dropping
    // it on Windows invalidates the master's input pipe (wezterm #4206).
    let slave = pair.slave;
    std::thread::spawn(move || {
        let _keep_slave_alive = slave;
        let mut closed_by_user = false;
        loop {
            if close_rx.try_recv().is_ok() {
                closed_by_user = true;
                let _ = child.kill();
            }
            if let Ok(Some(status)) = child.try_wait() {
                let reason = if closed_by_user {
                    "closed".to_string()
                } else {
                    format!("exit {}", status.exit_code())
                };
                let _ = event_tx.send(SessionEvent::Closed { exit_code: Some(status.exit_code()), reason });
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    });

    Ok(SessionHandle {
        input_tx,
        resize_tx,
        output_rx,
        event_rx,
        close_tx,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Spawns the local shell through the REAL terminal model (as the GUI does):
    /// the model answers terminal queries (CPR etc.) automatically — clink sends
    /// ESC[6n on startup and blocks until answered.
    #[test]
    fn local_shell_smoke() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        std::env::set_var("XXSSHG_LOCAL_CMD", "cmd.exe");
        std::env::set_var("XXSSHG_LOCAL_ARGS", "/k");
        let kind = if cfg!(windows) { ShellKind::Cmd } else { ShellKind::DefaultShell };
        let mut handle = spawn(&rt.handle().clone(), kind, 80, 24).expect("spawn local shell");

        let input_tx = handle.input_tx.clone();
        let (mut term, _title, _bell) =
            crate::term::Terminal::new(80, 24, 1000, input_tx);

        let mut got = false;
        let mut all: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        // clink init can outrun the first input; retry the marker periodically
        let mut last_send = std::time::Instant::now();
        while std::time::Instant::now() < deadline && !got {
            rt.block_on(async {
                tokio::select! {
                    out = handle.output_rx.recv() => {
                        if let Some(bytes) = out {
                            // diagnostic build: re-disable win32-input-mode when ConPTY re-enables it
                            if bytes.windows(8).any(|w| w == b"[?9001h") {
                                let _ = handle.input_tx.send(b"[?9001l".to_vec());
                            }
                            term.feed(&bytes);
                            all.extend_from_slice(&bytes);
                            if all.windows(16).any(|w| w == b"xxsshg_local_ok") {
                                got = true;
                            }
                        }
                    }
                    ev = handle.event_rx.recv() => {
                        eprintln!("[diag] session event: {:?}", ev);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            });
            if !got && last_send.elapsed() >= std::time::Duration::from_secs(2) {
                eprintln!("[diag] tick, bytes={}", all.len());
                last_send = std::time::Instant::now();
            }
        }
        use std::io::Write as _;
        let mut f = std::fs::File::create("pty-local-dump.bin").unwrap();
        let _ = f.write_all(&all);
        let _ = handle.close_tx.send(());
        assert!(got, "never saw the echo marker; got: {}", String::from_utf8_lossy(&all));
    }
}
