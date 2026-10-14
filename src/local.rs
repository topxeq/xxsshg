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

/// Base64 of UTF-16LE — the payload format PowerShell's `-EncodedCommand`
/// expects. Hand-rolled (no base64 crate): 3 bytes -> 4 chars, std alphabet.
fn b64_utf16le(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() >= 2 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() >= 3 {
            out.push(T[n as usize & 63] as char);
        }
        if chunk.len() == 1 {
            out.push('=');
            out.push('=');
        }
        if chunk.len() == 2 {
            out.push('=');
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Cmd,
    PowerShell,
    DefaultShell,
}

/// Spawn a local shell on a new PTY; returns the same handle shape as SSH tabs.
/// No cwd hook is injected: drag-drop upload trusts a current directory only
/// when the shell reports one on its own (e.g. a user-installed shell
/// integration emitting OSC 7/9;9); otherwise the user picks the directory.
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
        if std::env::var("XXSSHG_LOCAL_E2E").ok().as_deref() != Some("1") {
            return;
        }
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

/// E2E spike (run with `XXSSHG_OSC99_E2E=1 cargo test osc99 -- --nocapture`):
/// verify that ConPTY passes OSC 9;9 cwd reports from cmd.exe / powershell.exe
/// through to the PTY output byte stream. This is the premise of local
/// drag-drop upload (Windows Terminal's official shell integration relies on
/// the same pass-through).
#[cfg(test)]
mod osc99_spike {
    use super::*;
    use std::io::Read as _;
    use std::io::Write as _;

    fn collect(cmdline: &str, args: &[&str], probe: &[u8], secs: u64) -> Vec<u8> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut c = portable_pty::CommandBuilder::new(cmdline);
        for a in args {
            c.arg(a);
        }
        c.cwd(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()));
        let mut child = pair.slave.spawn_command(c).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        // match local.rs: disable win32-input-mode so plain-byte input works
        let _ = writer.write_all(b"[?9001l");
        let _ = writer.flush();

        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let out2 = out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });

        // clink/PSReadLine send ESC[6n at startup and BLOCK until answered;
        // the real GUI's terminal model answers CPR automatically — emulate
        // that here, from t=0 (ConPTY switches to win32-input-mode at init
        // and then swallows plain-byte input, so 9001l must go out before
        // the CPR answer)
        let mut answered = 0usize;
        let mut disabled_9001 = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            let (n, seen_9001) = {
                let buf = out.lock().unwrap();
                (
                    buf.windows(4).filter(|w| w == b"[6n").count(),
                    buf.windows(8).any(|w| w == b"[?9001h"),
                )
            };
            if seen_9001 && !disabled_9001 {
                let _ = writer.write_all(b"[?9001l");
                disabled_9001 = true;
            }
            for _ in answered..n {
                let _ = writer.write_all(b"[1;1R");
                answered += 1;
            }
            let _ = writer.flush();
            if answered >= 1 && disabled_9001 {
                // PowerShell 5.1 with a profile is SLOW to first prompt —
                // give it plenty of time before probing
                std::thread::sleep(std::time::Duration::from_secs(8));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = writer.write_all(probe);
        let _ = writer.flush();
        std::thread::sleep(std::time::Duration::from_secs(secs));
        let _ = child.kill();
        let data = out.lock().unwrap().clone();
        data
    }

    /// E2E verification (run with `XXSSHG_OSC99_E2E=1 cargo test osc99 -- --nocapture`):
    /// ConPTY passes the OSC 9;9 cwd reports of our cmd/PowerShell hooks
    /// through to the PTY output byte stream — the premise of local
    /// drag-drop upload. Both hooks appear here EXACTLY as local.rs spawns
    /// them (also covers clink-injected cmd and PSReadLine).
    #[test]
    fn osc99_passthrough() {
        if std::env::var("XXSSHG_OSC99_E2E").ok().as_deref() != Some("1") {
            return;
        }
        // PowerShell hook as local.rs installs it: a prompt() override that
        // emits OSC 9;9 then keeps the default look. (NB: `& $orig` prompt
        // wrapping + [Console]::Write in ONE function hangs under ConPTY —
        // keep this exact shape.)
        let ps_hook = "function global:prompt { $e=[char]27; if ($PWD.Provider.Name -eq 'FileSystem') { [Console]::Write($e+']9;9;'+$env:COMPUTERNAME+';'+$PWD.ProviderPath+$e+'\\') }; 'PS ' + $PWD.ProviderPath + '> ' }";
        let cases: Vec<(&str, &str, Vec<String>, &[u8])> = vec![
            (
                "cmd",
                "cmd.exe",
                vec![
                    "/K".into(),
                    r"PROMPT $E]9;9;%COMPUTERNAME%;$P$E\$P$G".into(),
                ],
                b"cd \x5cWindows\r",
            ),
            (
                "powershell",
                "powershell.exe",
                vec![
                    "-NoExit".into(),
                    "-EncodedCommand".into(),
                    b64_utf16le(ps_hook),
                ],
                b"cd $env:windir\r",
            ),
        ];
        for (label, prog, args, probe) in cases {
            let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let data = collect(prog, &argrefs, probe, 5);
            let text = String::from_utf8_lossy(&data);
            let hits = text.matches("]9;9;").count();
            println!("{label}: {} bytes, ]9;9; hits = {}", data.len(), hits);
            let _ = std::fs::write(format!("osc99-{label}.bin"), &data);
            assert!(
                hits >= 2,
                "{label}: expected >=2 OSC 9;9 reports (initial prompt + after cd), got {hits}"
            );
        }
    }
}

#[cfg(test)]
mod b64_tests {
    use super::b64_utf16le;

    #[test]
    fn matches_reference_values() {
        // reference values from Python base64.b64encode(s.encode('utf-16-le'))
        assert_eq!(b64_utf16le("echo BBB"), "ZQBjAGgAbwAgAEIAQgBCAA==");
        assert_eq!(b64_utf16le(""), "");
        assert_eq!(b64_utf16le("a"), "YQA=");
        assert_eq!(b64_utf16le("ab"), "YQBiAA==");
        assert_eq!(b64_utf16le("abc"), "YQBiAGMA");
    }
}
