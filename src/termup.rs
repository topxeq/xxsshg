//! Drag & drop upload for terminal tabs.
//!
//! A terminal tab has no SFTP channel of its own; on the first drop one is
//! opened lazily as a subsystem channel on the tab's EXISTING SSH connection
//! (no second TCP connection, no re-auth: servers with connection limits or
//! per-source penalties reset extra connections with os error 10054). The
//! upload target is the terminal's current directory ONLY when the shell
//! reported it via OSC 7 and the report passes the trust checks in
//! `Terminal::trusted_cwd`; otherwise a remote directory picker dialog
//! anchored at the user's home directory asks for the target.
//!
//! Target semantics match the SFTP tab's `upload_path`: the target is a
//! container directory, the item's own name is always appended
//! (`/mnt1/dest` + `d:\tmp\abc` -> `/mnt1/dest/abc`).

use std::path::PathBuf;
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use tokio::sync::{mpsc, oneshot};

use crate::i18n::{tpl, tr, Language};
use crate::sftp::{self, FileEntry};
use crate::sftpui::{Transfer, TransferKind};
use crate::session::SessionHandle;

/// Connection state of the terminal tab's lazy SFTP channel
pub enum UpState {
    Idle,
    Connecting {
        status: String,
        started: std::time::Instant,
        result_rx: oneshot::Receiver<Result<Arc<tokio::sync::Mutex<SftpSession>>, String>>,
    },
    Ready,
    Failed(String),
}

/// Result of validating the directory typed/chosen in the picker
pub enum PickMsg {
    /// directory exists: start the upload into it
    Exists(String),
    /// path does not exist: ask whether to create it first
    NeedCreate(String),
    /// validation failed (e.g. path is a file)
    Error(String),
}

/// Directory picker dialog state (target unresolved -> ask the user)
pub struct DirPick {
    /// dropped items waiting for a target directory
    pub paths: Vec<PathBuf>,
    /// currently browsed directory (absolute)
    pub dir: String,
    /// path text input (authoritative for OK)
    pub input: String,
    /// subdirectories of `dir`
    pub entries: Vec<FileEntry>,
    pub loading: bool,
    pub error: Option<String>,
    /// validation / creation request in flight
    pub busy: bool,
    /// OK hit on a non-existing dir: show the create-and-upload confirm
    pub confirm_create: bool,
    pub create_dir: String,
    list_rx: Option<mpsc::UnboundedReceiver<Result<(String, Vec<FileEntry>), String>>>,
    ok_rx: Option<mpsc::UnboundedReceiver<PickMsg>>,
}

pub struct TermUp {
    pub lang: Language,
    /// true = local shell tab: targets are local directories and "upload" is
    /// a plain copy; false = SSH tab: SFTP upload (channel opened lazily)
    pub local: bool,
    pub state: UpState,
    pub sftp: Option<Arc<tokio::sync::Mutex<SftpSession>>>,
    /// remote user's home directory (canonicalize "."), when resolved
    pub home: Option<String>,
    home_rx: Option<oneshot::Receiver<Option<String>>>,
    /// items dropped while the SFTP channel was not ready yet
    pub pending: Vec<PathBuf>,
    pub pick: Option<DirPick>,
    pub transfers: Vec<Transfer>,
    /// transient overlay message (direct-upload target / results)
    pub toast: Option<(String, std::time::Instant)>,
    prog_tx: mpsc::UnboundedSender<(u64, u64, u64, Option<String>)>,
    prog_rx: mpsc::UnboundedReceiver<(u64, u64, u64, Option<String>)>,
    done_tx: mpsc::UnboundedSender<(u64, Option<String>, Option<sftp::TransferStats>)>,
    done_rx: mpsc::UnboundedReceiver<(u64, Option<String>, Option<sftp::TransferStats>)>,
    ask_tx: mpsc::UnboundedSender<(String, oneshot::Sender<u8>)>,
    ask_rx: mpsc::UnboundedReceiver<(String, oneshot::Sender<u8>)>,
    /// conflict dialog currently shown (one at a time, rest queue up)
    pub transfer_ask: Option<(String, oneshot::Sender<u8>)>,
    next_id: u64,
}

impl TermUp {
    pub fn new(lang: Language, local: bool) -> Self {
        let (prog_tx, prog_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let (ask_tx, ask_rx) = mpsc::unbounded_channel();
        Self {
            lang,
            local,
            // local tabs copy directly: the channel is always "ready"
            state: if local { UpState::Ready } else { UpState::Idle },
            sftp: None,
            home: None,
            home_rx: None,
            pending: Vec::new(),
            pick: None,
            transfers: Vec::new(),
            toast: None,
            prog_tx,
            prog_rx,
            done_tx,
            done_rx,
            ask_tx,
            ask_rx,
            transfer_ask: None,
            next_id: 1,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, UpState::Ready)
    }

    pub fn connecting_status(&self) -> Option<&str> {
        match &self.state {
            UpState::Connecting { status, .. } => Some(status),
            _ => None,
        }
    }

    pub fn note(&mut self, msg: String) {
        self.toast = Some((msg, std::time::Instant::now()));
    }

    /// Surface a failed channel setup (shown once as a toast); the state
    /// resets so the next drop starts a fresh attempt. Pending drops are
    /// discarded — auto-retrying an auth that just failed would loop.
    pub fn take_failure(&mut self) -> Option<String> {
        // GUARD: only reset when actually Failed. This runs EVERY frame; an
        // unconditional `mem::replace(.., Idle)` here clobbered Ready and
        // Connecting — the kick then re-opened the channel per frame (the
        // connection flood behind the original os error 10054, and the
        // "opening upload channel" hang).
        if !matches!(self.state, UpState::Failed(_)) {
            return None;
        }
        match std::mem::replace(&mut self.state, UpState::Idle) {
            UpState::Failed(msg) => {
                crate::diag::uplog(&format!("take_failure: {msg} (pending cleared)"));
                self.pending.clear();
                Some(msg)
            }
            _ => unreachable!("state was Failed just above"),
        }
    }

    /// Queue dropped items; consumed once the SFTP channel is ready.
    pub fn enqueue(&mut self, paths: Vec<PathBuf>) {
        crate::diag::uplog(&format!(
            "enqueue: {} item(s) (state={}, pending now {})",
            paths.len(),
            self.state_name(),
            self.pending.len() + paths.len()
        ));
        self.pending.extend(paths);
    }

    /// Short state label for tracing
    pub fn state_name(&self) -> &'static str {
        match self.state {
            UpState::Idle => "Idle",
            UpState::Connecting { .. } => "Connecting",
            UpState::Ready => "Ready",
            UpState::Failed(_) => "Failed",
        }
    }

    /// Open the lazy SFTP subsystem channel on the tab's existing SSH
    /// connection (no-op while already connecting/ready)
    pub fn connect_via_session(&mut self, handle: &SessionHandle, host: &str) {
        if !matches!(self.state, UpState::Idle | UpState::Failed(_)) {
            return;
        }
        let (result_tx, result_rx) = oneshot::channel();
        let sent = handle.sftp_req_tx.send(result_tx);
        crate::diag::uplog(&format!(
            "connect_via_session: request sent to session task (send_err={})", sent.is_err()
        ));
        let status = tpl(tr(self.lang, "up_connecting"), &[("host", host)]);
        self.state = UpState::Connecting { status, started: std::time::Instant::now(), result_rx };
    }

    /// Polled connect result (None while still in flight)
    pub fn poll_result(&mut self) -> Option<Result<Arc<tokio::sync::Mutex<SftpSession>>, String>> {
        if let UpState::Connecting { result_rx, .. } = &mut self.state {
            result_rx.try_recv().ok()
        } else {
            None
        }
    }

    /// Apply a finished connect result (polled by the app each frame)
    pub fn on_connect_result(&mut self, rt: &tokio::runtime::Handle, res: Result<Arc<tokio::sync::Mutex<SftpSession>>, String>) {
        crate::diag::uplog(&format!(
            "on_connect_result: {}{}",
            if res.is_ok() { "Ready" } else { "Failed" },
            res.as_ref().err().map(|e| format!(" ({e})")).unwrap_or_default()
        ));
        match res {
            Ok(sftp) => {
                self.sftp = Some(sftp);
                self.state = UpState::Ready;
                // resolve the user's home directory in the background (the
                // picker anchors there; canonicalize "." is the server's own
                // answer, not a guess)
                if self.home.is_none() {
                    let sftp = self.sftp.clone().unwrap();
                    let (tx, rx) = oneshot::channel();
                    self.home_rx = Some(rx);
                    rt.spawn(async move {
                        let g = sftp.lock().await;
                        let home = g.canonicalize(".").await.ok();
                        drop(g);
                        let _ = tx.send(home);
                    });
                }
            }
            Err(e) => {
                self.state = UpState::Failed(e);
            }
        }
    }

    /// Drain progress / completion / conflict queues. Cheap, every frame.
    pub fn update(&mut self) {
        while let Ok((id, done, total, err)) = self.prog_rx.try_recv() {
            if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                t.done = done;
                if total != u64::MAX {
                    t.total = total;
                }
                if let Some(e) = err {
                    t.err = Some(e);
                    t.finished = true;
                }
            }
        }
        while let Ok((id, err, stats)) = self.done_rx.try_recv() {
            let (name, secs) = match self.transfers.iter().find(|t| t.id == id) {
                Some(t) => (t.name.clone(), t.started.elapsed().as_secs()),
                None => (String::new(), 0),
            };
            // the entry's only remaining consumer is the toast — prune it so
            // the list stays bounded to in-flight transfers
            self.transfers.retain(|t| t.id != id);
            match (&err, &stats) {
                (None, Some(s)) => {
                    let extra = if s.skipped > 0 {
                        format!(", {}", tpl(tr(self.lang, "sftp_log_skipped"), &[("n", &s.skipped.to_string())]))
                    } else {
                        String::new()
                    };
                    self.note(format!(
                        "✓ {} ({}{}, {}s)",
                        name,
                        sftp::fmt_size_pub(s.bytes),
                        extra,
                        secs
                    ));
                }
                (None, None) => self.note(format!("✓ {name}")),
                (Some(e), _) => self.note(format!("✗ {name}: {e}")),
            }
        }
        // watchdog: a channel request that never completes must not hang the
        // badge forever. Fail visibly; the next drop retries from Idle.
        if let UpState::Connecting { started, .. } = &self.state {
            if started.elapsed() > std::time::Duration::from_secs(30) {
                crate::diag::uplog("watchdog: Connecting exceeded 30s -> Failed (retryable)");
                self.state = UpState::Failed(tr(self.lang, "up_timeout").to_string());
                self.pending.clear();
            }
        }
        // one conflict dialog at a time; the rest queue up
        if self.transfer_ask.is_none() {
            if let Ok(q) = self.ask_rx.try_recv() {
                self.transfer_ask = Some(q);
            }
        }
        // home directory resolution
        if let Some(mut rx) = self.home_rx.take() {
            match rx.try_recv() {
                Ok(home) => {
                    self.home = home.or(self.home.take());
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.home_rx = Some(rx);
                }
                Err(oneshot::error::TryRecvError::Closed) => {}
            }
        }
    }

    /// Upload/copy dropped items into the container directory `dir` — the
    /// item's own name is appended (same semantics as the SFTP tab's
    /// upload_path). Routes to the SFTP engine or the local copy engine.
    pub fn upload_to(&mut self, rt: &tokio::runtime::Handle, dir: &str, paths: Vec<PathBuf>) {
        crate::diag::uplog(&format!(
            "upload_to: {} item(s) into {:?} ({})",
            paths.len(),
            dir,
            if self.local { "local copy" } else { "sftp" }
        ));
        if self.local {
            self.upload_local(rt, dir, paths);
        } else {
            self.upload_sftp(rt, dir, paths);
        }
    }

    /// Local copy into container directory `dir` (a Windows path)
    fn upload_local(&mut self, rt: &tokio::runtime::Handle, dir: &str, paths: Vec<PathBuf>) {
        for lpath in paths {
            let Ok(md) = std::fs::metadata(&lpath) else {
                self.note(format!("✗ {}", lpath.display()));
                continue;
            };
            let is_dir = md.is_dir();
            let Some(name) = lpath.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let rtarget = std::path::Path::new(dir).join(&name);
            let id = self.next_id;
            self.next_id += 1;
            let total = if is_dir {
                sftp::count_local_bytes(&lpath).1
            } else {
                md.len()
            };
            self.transfers.push(Transfer {
                id,
                kind: TransferKind::Upload,
                name: name.clone(),
                total,
                done: 0,
                err: None,
                finished: false,
                started: std::time::Instant::now(),
            });
            let prog_tx = self.prog_tx.clone();
            let done_tx = self.done_tx.clone();
            let policy = sftp::ConflictPolicy::new(self.ask_tx.clone());
            rt.spawn(async move {
                let _ = prog_tx.send((id, 0, total, None));
                let prog_tx2 = prog_tx.clone();
                let prog = move |done: u64| {
                    let _ = prog_tx2.send((id, done.min(total), total, None));
                };
                let r = copy_local(&lpath, &rtarget, &prog, &policy).await;
                let (err, stats) = match r {
                    Ok(s) => (None, Some(s)),
                    Err(e) => (Some(e), None),
                };
                let _ = prog_tx.send((id, total, total, err.clone()));
                let _ = done_tx.send((id, err, stats));
            });
        }
    }

    /// SFTP upload into container directory `dir` (remote path)
    fn upload_sftp(&mut self, rt: &tokio::runtime::Handle, dir: &str, paths: Vec<PathBuf>) {
        let Some(sftp) = self.sftp.clone() else { return };
        for lpath in paths {
            let Ok(md) = std::fs::metadata(&lpath) else {
                self.note(format!("✗ {}", lpath.display()));
                continue;
            };
            let is_dir = md.is_dir();
            let Some(name) = lpath.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let rtarget = sftp::join_remote(dir, &name);
            let id = self.next_id;
            self.next_id += 1;
            let total = if is_dir {
                sftp::count_local_bytes(&lpath).1
            } else {
                md.len()
            };
            self.transfers.push(Transfer {
                id,
                kind: TransferKind::Upload,
                name: name.clone(),
                total,
                done: 0,
                err: None,
                finished: false,
                started: std::time::Instant::now(),
            });
            let prog_tx = self.prog_tx.clone();
            let done_tx = self.done_tx.clone();
            let policy = sftp::ConflictPolicy::new(self.ask_tx.clone());
            let sftp = sftp.clone();
            rt.spawn(async move {
                let _ = prog_tx.send((id, 0, total, None));
                let prog_tx2 = prog_tx.clone();
                let prog = move |done: u64| {
                    let _ = prog_tx2.send((id, done.min(total), total, None));
                };
                let g = sftp.lock().await;
                let r = if is_dir {
                    sftp::upload_dir_recursive(&g, &lpath, &rtarget, &prog, &policy).await
                } else {
                    sftp::upload_file(&g, &lpath, &rtarget, total, &prog, &policy).await
                };
                drop(g);
                let (err, stats) = match r {
                    Ok(s) => (None, Some(s)),
                    Err(e) => (Some(e), None),
                };
                let _ = prog_tx.send((id, total, total, err.clone()));
                let _ = done_tx.send((id, err, stats));
            });
        }
    }

    // -- directory picker ---------------------------------------------------

    /// Open the picker for `paths`, anchored at `anchor` (remote home)
    pub fn pick_open(&mut self, rt: &tokio::runtime::Handle, paths: Vec<PathBuf>, anchor: Option<String>) {
        crate::diag::uplog(&format!(
            "pick_open (remote): {} item(s), anchor={:?}",
            paths.len(),
            anchor
        ));
        let dir = anchor.unwrap_or_else(|| "/".into());
        self.pick = Some(DirPick {
            paths,
            dir: dir.clone(),
            input: dir.clone(),
            entries: Vec::new(),
            loading: false,
            error: None,
            busy: false,
            confirm_create: false,
            create_dir: String::new(),
            list_rx: None,
            ok_rx: None,
        });
        self.pick_nav(rt, &dir);
    }

    /// Open the LOCAL picker, anchored at the user's home directory
    pub fn pick_open_local(&mut self, rt: &tokio::runtime::Handle, paths: Vec<PathBuf>, anchor: Option<PathBuf>) {
        crate::diag::uplog(&format!(
            "pick_open_local: {} item(s), anchor={:?}",
            paths.len(),
            anchor
        ));
        let dir = anchor
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "C:\\".into());
        self.pick = Some(DirPick {
            paths,
            dir: dir.clone(),
            input: dir.clone(),
            entries: Vec::new(),
            loading: false,
            error: None,
            busy: false,
            confirm_create: false,
            create_dir: String::new(),
            list_rx: None,
            ok_rx: None,
        });
        self.pick_nav(rt, &dir);
    }

    /// Browse to `dir` (canonicalized; also used for Up via `dir/..`)
    pub fn pick_nav(&mut self, rt: &tokio::runtime::Handle, dir: &str) {
        let Some(pick) = &mut self.pick else { return };
        pick.loading = true;
        pick.error = None;
        pick.confirm_create = false;
        let (tx, rx) = mpsc::unbounded_channel();
        pick.list_rx = Some(rx);
        let dir = dir.to_string();
        if self.local {
            rt.spawn(async move {
                let res = async {
                    tokio::task::spawn_blocking(move || {
                        let p = std::path::PathBuf::from(&dir);
                        let resolved = std::fs::canonicalize(&p).map_err(|e| e.to_string())?;
                    // canonicalize yields the \\?\C:\... extended form — ugly
                    // in the input box; everything accepts the plain form
                    let resolved = resolved.to_string_lossy().into_owned();
                    let resolved = resolved
                        .strip_prefix(r"\\?\")
                        .map(|s| s.to_string())
                        .unwrap_or(resolved);
                    let mut dirs: Vec<FileEntry> = std::fs::read_dir(&resolved)
                        .map_err(|e| e.to_string())?
                        .flatten()
                        .filter_map(|e| e.file_type().ok().map(|ft| (e, ft)))
                        .filter(|(_, ft)| ft.is_dir())
                        .map(|(e, _)| FileEntry {
                            name: e.file_name().to_string_lossy().into_owned(),
                            is_dir: true,
                            size: 0,
                            mtime: 0,
                            ctime: 0,
                        })
                        .collect();
                        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        Ok((resolved, dirs))
                    })
                    .await
                    .map_err(|e| e.to_string())?
                }
                .await;
                let _ = tx.send(res);
            });
            return;
        }
        let Some(sftp) = self.sftp.clone() else { return };
        rt.spawn(async move {
            let g = sftp.lock().await;
            let res = async {
                let resolved = g.canonicalize(&dir).await.map_err(|e| e.to_string())?;
                let entries = sftp::list_dir(&g, &resolved).await?;
                let mut dirs: Vec<FileEntry> = entries.into_iter().filter(|e| e.is_dir).collect();
                dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                Ok((resolved, dirs))
            }
            .await;
            drop(g);
            let _ = tx.send(res);
        });
    }

    /// OK pressed on the path input: validate in the background
    pub fn pick_ok(&mut self, rt: &tokio::runtime::Handle) {
        let Some(pick) = &mut self.pick else { return };
        let dir = pick.input.trim().to_string();
        if dir.is_empty() {
            return; // nothing typed: treat OK as a no-op
        }
        pick.busy = true;
        pick.error = None;
        let (tx, rx) = mpsc::unbounded_channel();
        pick.ok_rx = Some(rx);
        if self.local {
            rt.spawn(async move {
                let msg = if std::path::Path::new(&dir).is_dir() {
                    PickMsg::Exists(dir)
                } else {
                    PickMsg::NeedCreate(dir)
                };
                let _ = tx.send(msg);
            });
            return;
        }
        let Some(sftp) = self.sftp.clone() else { return };
        rt.spawn(async move {
            let g = sftp.lock().await;
            let msg = match sftp::remote_is_dir(&g, &dir).await {
                Some(true) => PickMsg::Exists(dir),
                Some(false) => PickMsg::Error(format!("{dir}: not a directory")),
                None => PickMsg::NeedCreate(dir),
            };
            drop(g);
            let _ = tx.send(msg);
        });
    }

    /// "Create & upload" confirmed on a non-existing directory
    pub fn pick_create(&mut self, rt: &tokio::runtime::Handle) {
        let Some(pick) = &mut self.pick else { return };
        // keep create_dir: on failure the confirm view must stay reproducible
        let dir = pick.create_dir.clone();
        pick.busy = true;
        pick.error = None;
        let (tx, rx) = mpsc::unbounded_channel();
        pick.ok_rx = Some(rx);
        if self.local {
            rt.spawn(async move {
                let msg = match std::fs::create_dir_all(&dir) {
                    Ok(()) => PickMsg::Exists(dir),
                    Err(e) => PickMsg::Error(e.to_string()),
                };
                let _ = tx.send(msg);
            });
            return;
        }
        let Some(sftp) = self.sftp.clone() else { return };
        rt.spawn(async move {
            let g = sftp.lock().await;
            let msg = match sftp::mkdir_all(&g, &dir).await {
                Ok(()) => PickMsg::Exists(dir),
                Err(e) => PickMsg::Error(e),
            };
            drop(g);
            let _ = tx.send(msg);
        });
    }

    /// Drain picker channels; returns `Some((target_dir, paths))` when the
    /// user confirmed an existing directory — the app then uploads.
    pub fn poll_pick(&mut self) -> Option<(String, Vec<PathBuf>)> {
        let mut out = None;
        if let Some(pick) = &mut self.pick {
            if let Some(rx) = &mut pick.list_rx {
                loop {
                    match rx.try_recv() {
                        Ok(Ok((dir, dirs))) => {
                            pick.dir = dir.clone();
                            pick.input = dir;
                            pick.entries = dirs;
                            pick.loading = false;
                        }
                        Ok(Err(e)) => {
                            pick.error = Some(e);
                            pick.loading = false;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            pick.list_rx = None;
                            break;
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                    }
                }
            }
            if let Some(rx) = &mut pick.ok_rx {
                match rx.try_recv() {
                    Ok(PickMsg::Exists(dir)) => {
                        pick.busy = false;
                        crate::diag::uplog(&format!(
                            "pick confirmed: {} item(s) into {dir}",
                            pick.paths.len()
                        ));
                        out = Some((dir, std::mem::take(&mut pick.paths)));
                    }
                    Ok(PickMsg::NeedCreate(dir)) => {
                        pick.busy = false;
                        pick.confirm_create = true;
                        pick.create_dir = dir;
                    }
                    Ok(PickMsg::Error(e)) => {
                        pick.busy = false;
                        // leave the confirm-create view so the error (shown in
                        // the main view) is visible and OK can be retried
                        pick.confirm_create = false;
                        pick.error = Some(e);
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        pick.ok_rx = None;
                        pick.busy = false;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
            }
        }
        if out.is_some() {
            self.pick = None;
        }
        out
    }

    /// Draw the picker window. Returns the same confirmation as `poll_pick`
    /// (Exists results are turned into uploads by the caller).
    pub fn pick_ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle) {
        let Some(pick) = &mut self.pick else { return };
        let lang = self.lang;
        let is_local = self.local;
        let mut nav: Option<String> = None;
        let mut ok = false;
        let mut cancel = false;
        let mut create = false;
        let mut open = true;
        egui::Window::new(tpl(tr(lang, if is_local { "up_pick_title_copy" } else { "up_pick_title" }), &[]))
            .id(egui::Id::new("termup_pick_win"))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ui, |ui| {
                if pick.confirm_create {
                    ui.label(tpl(
                        tr(lang, "up_dir_missing"),
                        &[("dir", &pick.create_dir)],
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(tpl(tr(lang, "up_create_upload"), &[])).clicked() {
                            create = true;
                        }
                        if ui.button(tpl(tr(lang, "up_cancel"), &[])).clicked() {
                            pick.confirm_create = false;
                        }
                    });
                    return; // early: nothing else while confirming
                }
                ui.horizontal(|ui| {
                    let go = ui.add(
                        egui::TextEdit::singleline(&mut pick.input)
                            .desired_width(330.0)
                            .hint_text("/mnt1/dest"),
                    );
                    let enter = go.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button(tpl(tr(lang, "sftp_go"), &[])).clicked() || enter {
                        nav = Some(pick.input.trim().to_string());
                    }
                    if ui.button(tpl(tr(lang, "sftp_up"), &[])).clicked() {
                        nav = Some(sftp::join_remote(&pick.dir, ".."));
                    }
                });
                ui.separator();
                if pick.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tpl(tr(lang, "up_listing"), &[("dir", &pick.dir)]));
                    });
                } else if pick.entries.is_empty() {
                    ui.weak(tpl(tr(lang, "up_pick_empty"), &[]));
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .id_salt("termup_pick_list")
                        .show(ui, |ui| {
                            let entries = pick.entries.clone();
                            let cur = pick.dir.clone();
                            for e in entries {
                                let path = sftp::join_remote(&cur, &e.name);
                                let resp = ui.selectable_label(false, format!("{}/", e.name));
                                if resp.clicked() {
                                    pick.input = path.clone();
                                }
                                if resp.double_clicked() {
                                    nav = Some(path);
                                }
                            }
                        });
                }
                if let Some(err) = &pick.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err.clone());
                }
                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(tpl(tr(lang, "up_cancel"), &[])).clicked() {
                        cancel = true;
                    }
                    if pick.busy {
                        ui.spinner();
                    } else if ui
                        .add(egui::Button::new(tpl(tr(lang, if is_local { "up_copy_ok" } else { "up_pick_ok" }), &[])))
                        .clicked()
                    {
                        ok = true;
                    }
                });
            });
        if !open {
            self.pick = None;
            return;
        }
        if let Some(dir) = nav {
            self.pick_nav(rt, &dir);
        }
        if ok {
            self.pick_ok(rt);
        }
        if create {
            self.pick_create(rt);
        }
        if cancel {
            self.pick = None;
        }
    }

    /// Any unfinished transfers (drives the progress overlay)
    pub fn active_transfers(&self) -> impl Iterator<Item = &Transfer> {
        self.transfers.iter().filter(|t| !t.finished)
    }

    /// Drop the SFTP channel with the tab (the channel closes when the last
    /// Arc lands — in-flight upload tasks keep it alive until they finish)
    pub fn shutdown(&mut self) {
        crate::diag::uplog("shutdown: tab closing, upload channel dropped");
        self.sftp = None;
        self.state = UpState::Idle;
        self.pending.clear();
        self.pick = None;
    }
}

// ---------------------------------------------------------------------------
// Local copy engine (drag-drop "upload" of local shell tabs)
// ---------------------------------------------------------------------------

/// Recursive local copy. Directories are merged (created when missing); file
/// name conflicts go through `policy` exactly like the SFTP transfers
/// (overwrite / skip / auto-rename). Progress is per copied byte.
async fn copy_local(
    src: &std::path::Path,
    dest: &std::path::Path,
    prog: &(dyn Fn(u64) + Send + Sync),
    policy: &sftp::ConflictPolicy,
) -> Result<sftp::TransferStats, String> {
    async fn rec(
        src: &std::path::Path,
        dest: &std::path::Path,
        prog: &(dyn Fn(u64) + Send + Sync),
        policy: &sftp::ConflictPolicy,
        done: &mut u64,
    ) -> Result<sftp::TransferStats, String> {
        let mut stats = sftp::TransferStats { files: 0, bytes: 0, skipped: 0 };
        if src.is_dir() {
            tokio::fs::create_dir_all(dest).await.map_err(|e| e.to_string())?;
            let mut rd = tokio::fs::read_dir(src).await.map_err(|e| e.to_string())?;
            while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
                let child = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let sub = Box::pin(rec(&child, &dest.join(name), prog, policy, done)).await?;
                stats.files += sub.files;
                stats.bytes += sub.bytes;
                stats.skipped += sub.skipped;
            }
            return Ok(stats);
        }
        let dest_str = dest.to_string_lossy().into_owned();
        let final_dest = match policy.decide(&dest_str).await {
            sftp::Decision::Skip => {
                stats.skipped += 1;
                return Ok(stats);
            }
            sftp::Decision::Rename => sftp::free_local_name(dest),
            sftp::Decision::Overwrite => dest.to_path_buf(),
        };
        match tokio::fs::copy(src, &final_dest).await {
            Ok(n) => {
                *done += n;
                stats.files += 1;
                stats.bytes += n;
                prog(*done);
                Ok(stats)
            }
            Err(e) => Err(e.to_string()),
        }
    }
    let mut done: u64 = 0;
    rec(src, dest, prog, policy, &mut done).await
}

// ---------------------------------------------------------------------------
// Injection snippet + shared conflict dialog
// ---------------------------------------------------------------------------

/// Transfer-conflict dialog (over the terminal area), same answers as the
/// SFTP tab's. `ask` holds the pending question across frames.
pub fn conflict_ask_ui(
    ui: &mut egui::Ui,
    lang: Language,
    ask: &mut Option<(String, oneshot::Sender<u8>)>,
) {
    let Some((dest, resp)) = ask.take() else { return };
    let mut choice: Option<u8> = None;
    egui::Window::new(tpl(tr(lang, "sftp_conflict_title"), &[]))
        .id(egui::Id::new("termup_conflict_win"))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .show(ui, |ui| {
            ui.label(tpl(tr(lang, "sftp_conflict_msg"), &[("dest", &dest)]));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(tpl(tr(lang, "sftp_ovw"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_OVERWRITE);
                }
                if ui.button(tpl(tr(lang, "sftp_skip"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_SKIP);
                }
                if ui.button(tpl(tr(lang, "sftp_rename"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_RENAME);
                }
            });
            ui.horizontal(|ui| {
                if ui.button(tpl(tr(lang, "sftp_ovw_all"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_OVERWRITE_ALL);
                }
                if ui.button(tpl(tr(lang, "sftp_skip_all"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_SKIP_ALL);
                }
                if ui.button(tpl(tr(lang, "sftp_rename_all"), &[])).clicked() {
                    choice = Some(sftp::ANSWER_RENAME_ALL);
                }
            });
        });
    match choice {
        Some(c) => {
            let _ = resp.send(c);
        }
        None => *ask = Some((dest, resp)),
    }
}

/// Floating overlays over the terminal area: drop hint while files hover,
/// connecting badge, active-transfer progress list and the result toast.
/// Pure painter (no widgets): the terminal below keeps every event.
pub fn overlay_ui(up: &mut TermUp, ui: &mut egui::Ui, rect: egui::Rect, drop_hint: bool) {
    let lang = up.lang;
    let painter = ui.painter();
    let mut y = rect.bottom() - 8.0;

    // toast (bottom-right, auto-expires)
    if let Some((msg, at)) = &up.toast {
        if at.elapsed() < std::time::Duration::from_millis(4000) {
            let galley = painter.layout_no_wrap(
                msg.clone(),
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );
            let size = galley.size();
            let pos = egui::pos2(rect.right() - size.x - 12.0, y - size.y);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(pos.x - 8.0, pos.y - 4.0),
                    egui::vec2(size.x + 16.0, size.y + 8.0),
                ),
                4.0,
                egui::Color32::from_rgba_unmultiplied(30, 34, 40, 220),
            );
            painter.galley(pos, galley, egui::Color32::WHITE);
            y = pos.y - 8.0;
        } else {
            up.toast = None;
        }
    }

    // active transfers (bottom-right stack above the toast)
    let active: Vec<Transfer> = up.active_transfers().cloned().collect();
    if !active.is_empty() {
        let w = 300.0_f32;
        let row_h = 20.0;
        let h = active.len() as f32 * row_h + 12.0;
        let panel = egui::Rect::from_min_size(
            egui::pos2(rect.right() - w - 10.0, y - h),
            egui::vec2(w, h),
        );
        painter.rect_filled(panel, 6.0, egui::Color32::from_rgba_unmultiplied(30, 34, 40, 210));
        for (i, t) in active.iter().enumerate() {
            let row = egui::pos2(panel.min.x + 8.0, panel.min.y + 6.0 + i as f32 * row_h);
            let pct = if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
            let name = if t.name.chars().count() > 28 {
                format!("{}…", t.name.chars().take(28).collect::<String>())
            } else {
                t.name.clone()
            };
            painter.text(
                row,
                egui::Align2::LEFT_TOP,
                name,
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
            let bar_min = egui::pos2(panel.min.x + 8.0, row.y + 14.0);
            let bar_size = egui::vec2(w - 16.0, 3.0);
            painter.rect_filled(
                egui::Rect::from_min_size(bar_min, bar_size),
                1.5,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 40),
            );
            painter.rect_filled(
                egui::Rect::from_min_size(bar_min, egui::vec2(bar_size.x * pct.clamp(0.0, 1.0), 3.0)),
                1.5,
                egui::Color32::from_rgb(74, 160, 246),
            );
        }
    }

    // connecting badge
    if let Some(status) = up.connecting_status() {
        let galley = painter.layout_no_wrap(
            format!("⇅ {status}"),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 180),
        );
        let size = galley.size();
        let pos = egui::pos2(rect.right() - size.x - 12.0, rect.top() + 8.0);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(pos.x - 8.0, pos.y - 3.0),
                egui::vec2(size.x + 16.0, size.y + 6.0),
            ),
            4.0,
            egui::Color32::from_rgba_unmultiplied(30, 34, 40, 190),
        );
        painter.galley(pos, galley, egui::Color32::WHITE);
    }

    // drop hint while OS files hover over the terminal
    if drop_hint {
        let green = egui::Color32::from_rgb(74, 246, 118);
        painter.rect_filled(rect, 8.0, egui::Color32::from_rgba_unmultiplied(74, 246, 118, 20));
        painter.rect_stroke(rect, 8.0, (2.5, green), egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            tpl(tr(lang, if up.local { "drop_hint_copy" } else { "drop_hint" }), &[]),
            egui::FontId::proportional(20.0),
            green,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_local_merges_and_conflicts() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let base = std::env::temp_dir().join(format!("xxsshg-copy-e2e-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "world").unwrap();
        // pre-existing conflict at the destination: must be overwritten
        std::fs::write(dst.join("a.txt"), "STALE").unwrap();

        // sticky overwrite-all policy: answers conflicts without a dialog
        // (ConflictPolicy::new would await a UI answer -> deadlock in tests)
        let policy = sftp::ConflictPolicy::overwrite_all();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter2 = counter.clone();
        let prog = move |n: u64| counter2.store(n, std::sync::atomic::Ordering::Relaxed);

        let stats = rt
            .block_on(copy_local(&src, &dst, &prog, &policy))
            .expect("copy_local");
        assert_eq!(stats.files, 2, "two files copied");
        assert_eq!(stats.bytes, 10, "5 + 5 bytes");
        assert_eq!(stats.skipped, 0);
        assert!(counter.load(std::sync::atomic::Ordering::Relaxed) >= 10, "progress reported");
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub").join("b.txt")).unwrap(),
            "world"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
