use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::sync::{mpsc, oneshot};

use crate::i18n::{tpl, tr, Language};
use crate::sftp::{self, FileEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Upload,
    Download,
}

#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub kind: TransferKind,
    pub name: String,
    pub total: u64,
    pub done: u64,
    pub err: Option<String>,
    pub finished: bool,
    pub started: std::time::Instant,
}

pub enum OpMsg {
    RemoteList { dir: String, entries: Vec<FileEntry> },
    Error(String),
    TransferDone {
        id: u64,
        err: Option<String>,
        stats: Option<sftp::TransferStats>,
        refresh_remote: bool,
        refresh_local: bool,
    },
    RemoteDirSet(String),
    /// append rows to the properties window with matching id (async dir-size results);
    /// `done` clears the "calculating" spinner (dir props arrive in two steps)
    PropsAppend { id: u64, rows: Vec<(String, String)>, done: bool },
    /// remote file fetched into a temp file: open with system app or in the text viewer
    TempReady { name: String, path: Option<PathBuf>, open: bool, err: Option<String> },
    /// local listing changed (background delete finished); carries the log line
    LocalRefresh { log: Option<String> },
}

/// which SFTP pane an action targets (sort bar etc.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmKind {    RemoteDelete,
    RemoteDeleteDir,
    LocalDelete,
    LocalDeleteDir,
    RemoteRename,
    RemoteMkdir,
    RemoteMkfile,
    LocalRename,
    LocalMkdir,
    LocalMkfile,
}

/// text preview window (quick view)
pub struct Viewer {
    pub title: String,
    pub text: String,
    pub note: Option<String>,
    pub wrap: bool,
}

/// properties window; rows are filled in asynchronously for folder sizes
#[derive(Clone)]
pub struct Props {
    pub id: u64,
    pub title: String,
    pub rows: Vec<(String, String)>,
    pub pending: bool,
}

/// text preview: bytes shown / download cap for remote previews
const VIEW_TEXT_CAP: u64 = 1024 * 1024;
const VIEW_DL_CAP: u64 = 16 * 1024 * 1024;
const TEMP_VIEW_DIR: &str = "xxsshg-view";

pub struct SftpTab {
    pub name: String,
    pub sftp: Arc<tokio::sync::Mutex<SftpSession>>,
    pub close_tx: oneshot::Sender<()>,
    pub rt: tokio::runtime::Handle,
    pub lang: Language,
    /// server this tab is connected to (for tab-menu reconnect)
    pub server: crate::xconfig::Server,

    pub local_dir: PathBuf,
    pub local_entries: Vec<FileEntry>,
    pub local_sel: Option<usize>,
    pub local_edit: String,

    pub remote_dir: String,
    pub remote_entries: Vec<FileEntry>,
    pub remote_sel: Option<usize>,
    pub remote_edit: String,
    /// list sort: (key, ascending) — applied on refresh and on header click
    pub local_sort: (sftp::SortKey, bool),
    pub remote_sort: (sftp::SortKey, bool),

    pub loading: bool,
    pub error: Option<String>,
    op_rx: mpsc::UnboundedReceiver<OpMsg>,
    op_tx: mpsc::UnboundedSender<OpMsg>,

    pub transfers: Vec<Transfer>,
    prog_rx: mpsc::UnboundedReceiver<(u64, u64, u64, Option<String>)>,
    prog_tx: mpsc::UnboundedSender<(u64, u64, u64, Option<String>)>,
    /// activity log lines (✓/✗ prefixed, capped, newest at the bottom)
    pub oplog: Vec<String>,
    /// transfer conflict questions (dest path -> UI answer)
    ask_tx: mpsc::UnboundedSender<(String, oneshot::Sender<u8>)>,
    ask_rx: mpsc::UnboundedReceiver<(String, oneshot::Sender<u8>)>,
    /// conflict dialog currently shown (waits for a button press)
    transfer_ask: Option<(String, oneshot::Sender<u8>)>,
    next_id: u64,

    confirm: Option<(ConfirmKind, String)>,
    confirm_input: String,
    /// remote op in flight (suppress parallel refreshes)
    busy: bool,
    /// focus fixups for prompt dialogs
    confirm_input_focus_done: bool,
    /// quick-view window state
    pub viewer: Option<Viewer>,
    /// properties window state
    pub props: Option<Props>,
    /// token to discard stale async property results
    props_id: u64,
}


impl SftpTab {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        sftp: Arc<tokio::sync::Mutex<SftpSession>>,
        close_tx: oneshot::Sender<()>,
        rt: tokio::runtime::Handle,
        lang: Language,
        local_root: PathBuf,
        server: crate::xconfig::Server,
    ) -> Self {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let (prog_tx, prog_rx) = mpsc::unbounded_channel();
        let (ask_tx, ask_rx) = mpsc::unbounded_channel();
        let mut st = Self {
            name,
            sftp: sftp.clone(),
            close_tx,
            rt,
            lang,
            server,
            local_dir: local_root.clone(),
            local_entries: Vec::new(),
            local_sel: None,
            local_edit: local_root.to_string_lossy().into_owned(),
            remote_dir: String::from("/"),
            remote_entries: Vec::new(),
            remote_sel: None,
            remote_edit: String::new(),
            local_sort: (sftp::SortKey::Name, true),
            remote_sort: (sftp::SortKey::Name, true),
            loading: true,
            error: None,
            op_rx,
            op_tx,
            transfers: Vec::new(),
            prog_rx,
            prog_tx,
            oplog: Vec::new(),
            ask_tx,
            ask_rx,
            transfer_ask: None,
            next_id: 1,
            confirm: None,
            confirm_input: String::new(),
            busy: false,
            confirm_input_focus_done: false,
            viewer: None,
            props: None,
            props_id: 0,
        };
        st.refresh_local();
        // initial remote listing: home dir via canonicalize(".")
        st.spawn_refresh_remote(Some("."), true);
        st
    }

    pub fn refresh_local(&mut self) {
        let dir = self.local_dir.clone();
        // keep the selection on the same file across refreshes
        let prev_sel = self
            .local_sel
            .and_then(|i| self.local_entries.get(i))
            .map(|e| e.name.clone());
        let mut entries: Vec<FileEntry> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let Ok(md) = e.metadata() else { continue };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);
                let ctime = md
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(mtime);
                entries.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: ft.is_dir(),
                    size: md.len(),
                    mtime,
                    ctime,
                });
            }
        }
        let (key, asc) = self.local_sort;
        sftp::sort_entries_by(&mut entries, key, asc);
        self.local_sel = prev_sel.and_then(|n| entries.iter().position(|e| e.name == n));
        self.local_entries = entries;
        self.local_edit = dir.to_string_lossy().into_owned();
    }

    pub fn spawn_refresh_remote(&mut self, dir: Option<&str>, resolve_cwd: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.loading = true;
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let start = dir
            .map(|d| d.to_string())
            .unwrap_or_else(|| self.remote_dir.clone());
        self.rt.spawn(async move {
            let g = sftp.lock().await;
            let mut start = start;
            if resolve_cwd {
                // resolve the REQUESTED path (e.g. "." on first listing,
                // "<dir>/.." for Up) so the address bar shows an absolute dir
                match g.canonicalize(&start).await {
                    Ok(resolved) => {
                        let _ = tx.send(OpMsg::RemoteDirSet(resolved.clone()));
                        start = resolved;
                    }
                    Err(e) => {
                        let _ = tx.send(OpMsg::Error(e.to_string()));
                        return;
                    }
                }
            }
            match crate::sftp::list_dir(&g, &start).await {
                Err(e) => {
                    let _ = tx.send(OpMsg::Error(e));
                }
                Ok(entries) => {
                    let _ = tx.send(OpMsg::RemoteList { dir: start, entries });
                }
            }
        });
    }


    /// Drain op/progress messages; returns (refresh_remote, refresh_local)
    pub fn poll(&mut self) -> (bool, bool) {
        let mut refresh_remote = false;
        let mut refresh_local = false;
        // drain transfer progress ticks
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
        while let Ok(msg) = self.op_rx.try_recv() {
            match msg {
                OpMsg::RemoteList { dir, entries } => {
                    let mut entries = entries;
                    let (key, asc) = self.remote_sort;
                    sftp::sort_entries_by(&mut entries, key, asc);
                    self.remote_edit = dir.clone();
                    self.remote_dir = dir;
                    self.remote_entries = entries;
                    self.remote_sel = None;
                    self.loading = false;
                    self.busy = false;
                }
                OpMsg::Error(e) => {
                    self.error = Some(e.clone());
                    self.oplog_push(format!("✗ {}", e));
                    self.loading = false;
                    self.busy = false;
                }
                OpMsg::RemoteDirSet(d) => {
                    self.remote_edit = d.clone();
                    self.remote_dir = d;
                }
                OpMsg::LocalRefresh { log } => {
                    self.refresh_local();
                    if let Some(line) = log {
                        self.oplog_push(line);
                    }
                }
                OpMsg::TransferDone { id, err, stats, refresh_remote: rr, refresh_local: rl } => {
                    let line = self.transfer_log_line(id, err.as_deref(), stats);
                    self.oplog_push(line);
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.err = err;
                        t.finished = true;
                    }
                    refresh_remote |= rr;
                    refresh_local |= rl;
                }
                OpMsg::PropsAppend { id, rows, done } => {
                    if let Some(p) = &mut self.props {
                        if p.id == id {
                            p.rows.extend(rows);
                            if done {
                                p.pending = false;
                            }
                        }
                    }
                }
                OpMsg::TempReady { name, path, open, err } => {
                    if let Some(e) = err {
                        self.error = Some(tpl(tr(self.lang, "sftp_fetch_fail"), &[("name", &name), ("e", &e)]));
                    } else if let Some(p) = path {
                        if open {
                            if let Err(e) = open_with_system(&p) {
                                self.error = Some(tpl(tr(self.lang, "err_open_failed"), &[("name", &name), ("e", &e.to_string())]));
                            }
                        } else {
                            match read_text_preview(&p, &name, self.lang) {
                                Ok(v) => self.viewer = Some(v),
                                Err(e) => {
                                    self.error =
                                        Some(tpl(tr(self.lang, "err_open_failed"), &[("name", &name), ("e", &e.to_string())]));
                                }
                            }
                        }
                    }
                }
            }
        }
        // show one conflict dialog at a time; the rest queue up
        if self.transfer_ask.is_none() {
            if let Ok(q) = self.ask_rx.try_recv() {
                self.transfer_ask = Some(q);
            }
        }
        if refresh_remote {
            self.spawn_refresh_remote(None, false);
        }
        if refresh_local {
            self.refresh_local();
        }
        (refresh_remote, refresh_local)
    }

    fn start_transfer(&mut self, kind: TransferKind, name: String, total: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.transfers.push(Transfer {
            id,
            kind,
            name,
            total,
            done: 0,
            err: None,
            finished: false,
            started: std::time::Instant::now(),
        });
        id
    }

    /// append to the activity log, capping the buffer
    fn oplog_push(&mut self, line: String) {
        self.oplog.push(line);
        if self.oplog.len() > 300 {
            self.oplog.drain(..self.oplog.len() - 300);
        }
    }

    /// click handler for the sort bar: same key toggles direction, new key sorts asc
    fn set_sort(&mut self, pane: Pane, key: sftp::SortKey) {
        let (cur, asc) = match pane {
            Pane::Local => self.local_sort,
            Pane::Remote => self.remote_sort,
        };
        let next = if cur == key { (key, !asc) } else { (key, true) };
        match pane {
            Pane::Local => {
                self.local_sort = next;
                sftp::sort_entries_by(&mut self.local_entries, next.0, next.1);
            }
            Pane::Remote => {
                self.remote_sort = next;
                sftp::sort_entries_by(&mut self.remote_entries, next.0, next.1);
            }
        }
    }

    /// the clickable sort bar above a file list
    fn sort_bar(&mut self, ui: &mut egui::Ui, pane: Pane) {
        let (cur, asc) = match pane {
            Pane::Local => self.local_sort,
            Pane::Remote => self.remote_sort,
        };
        ui.horizontal_wrapped(|ui| {
            ui.weak(tpl(tr(self.lang, "sort_label"), &[]));
            for (key, label_key) in [
                (sftp::SortKey::Name, "sort_name"),
                (sftp::SortKey::Size, "sort_size"),
                (sftp::SortKey::Mtime, "sort_mtime"),
                (sftp::SortKey::Ctime, "sort_ctime"),
            ] {
                let active = cur == key;
                let label = if active {
                    // arrow indicates direction on the active key
                    format!("{} {}", tr(self.lang, label_key), if asc { "▲" } else { "▼" })
                } else {
                    tr(self.lang, label_key).to_string()
                };
                if ui
                    .selectable_label(active, egui::RichText::new(label).small())
                    .clicked()
                {
                    self.set_sort(pane, key);
                }
            }
        });
    }

    /// human-readable result line for a finished transfer
    fn transfer_log_line(
        &self,
        id: u64,
        err: Option<&str>,
        stats: Option<sftp::TransferStats>,
    ) -> String {
        let lang = self.lang;
        let Some(t) = self.transfers.iter().find(|t| t.id == id) else {
            return format!("✓ id={id}");
        };
        let secs = format!("{:.1}", t.started.elapsed().as_secs_f32());
        let action = tr(lang, if t.kind == TransferKind::Upload { "act_upload" } else { "act_download" });
        if let Some(e) = err {
            return format!(
                "✗ {}",
                tpl(tr(lang, "sftp_log_failed"), &[("action", action), ("name", &t.name), ("err", e)])
            );
        }
        let st = stats.unwrap_or_default();
        let mut line = tpl(
            tr(lang, if t.kind == TransferKind::Upload { "sftp_log_upload" } else { "sftp_log_download" }),
            &[
                ("name", t.name.as_str()),
                ("files", &st.files.to_string()),
                ("size", &sftp::fmt_size_pub(st.bytes)),
                ("secs", &secs),
            ],
        );
        if st.skipped > 0 {
            line += &tpl(tr(lang, "sftp_log_skipped"), &[("n", &st.skipped.to_string())]);
        }
        format!("✓ {line}")
    }



    /// Upload selected local file/dir into the current remote dir
    fn upload_selected(&mut self) {
        let Some(idx) = self.local_sel else { return };
        let Some(e) = self.local_entries.get(idx) else { return };
        self.upload_path(self.local_dir.join(&e.name), e.name.clone());
    }

    /// upload an arbitrary local path into the current remote dir — the toolbar,
    /// context menu and OS drag&drop all land here
    fn upload_path(&mut self, lpath: PathBuf, name: String) {
        let Ok(md) = std::fs::metadata(&lpath) else {
            let msg = format!("{} (missing)", lpath.display());
            self.error = Some(msg.clone());
            self.oplog_push(format!("✗ {msg}"));
            return;
        };
        let is_dir = md.is_dir();
        let rdir = self.remote_dir.clone();
        // target includes the item's own name: file -> remote file path,
        // dir -> remote folder created (merged if it already exists)
        let rtarget = sftp::join_remote(&rdir, &name);
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let id = self.start_transfer(
            TransferKind::Upload,
            name.clone(),
            if is_dir { 0 } else { md.len() },
        );
        let prog_id = id;
        let prog_tx = self.prog_tx.clone();
        let policy = sftp::ConflictPolicy::new(self.ask_tx.clone());
        self.rt.spawn(async move {
                let total = if is_dir {
                let (f, b) = sftp::count_local_bytes(&lpath);
                let _ = f;
                b
            } else {
                std::fs::metadata(&lpath).map(|m| m.len()).unwrap_or(0)
            };
            let _ = prog_tx.send((prog_id, 0, total, None));
                                    let prog_tx2 = prog_tx.clone();
            let prog = move |done: u64| {
                let _ = prog_tx2.send((prog_id, done.min(total), total, None));
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
            let _ = prog_tx.send((prog_id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err,
                stats,
                refresh_remote: true,
                refresh_local: false,
            });
        });
    }

    /// OS-level drag & drop: upload dropped files/folders into the current remote dir
    pub fn handle_os_dropped(&mut self, paths: Vec<PathBuf>) {
        let mut count = 0usize;
        for path in paths {
            let Some(name) = path.file_name().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            self.upload_path(path, name);
            count += 1;
        }
        if count > 0 {
            self.oplog_push(tpl(
                tr(self.lang, "sftp_log_dropped"),
                &[("n", &count.to_string())],
            ));
        }
    }

    /// Download selected remote file/dir into the current local dir
    fn download_selected(&mut self) {
        let Some(idx) = self.remote_sel else { return };
        let Some(e) = self.remote_entries.get(idx) else { return };
        let rname = e.name.clone();
        let is_dir = e.is_dir;
        let size = e.size;
        let rpath = sftp::join_remote(&self.remote_dir, &rname);
        let ldir = self.local_dir.clone();
        // target includes the item's own name: file -> local file path,
        // dir -> local folder recreated (merged if it already exists)
        let ltarget = ldir.join(&rname);
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let id = self.start_transfer(
            TransferKind::Download,
            rname.clone(),
            if is_dir { 0 } else { size },
        );
        let prog_tx = self.prog_tx.clone();
        let policy = sftp::ConflictPolicy::new(self.ask_tx.clone());
        self.rt.spawn(async move {
            let total = if is_dir {
                let g = sftp.lock().await;
                let (f, b) = sftp::count_remote_bytes(&g, &rpath).await;
                drop(g);
                let _ = f;
                b
            } else {
                size
            };
            let _ = prog_tx.send((id, 0, total, None));
                        let prog_tx2 = prog_tx.clone();
            let prog = move |done: u64| {
                let _ = prog_tx2.send((id, done.min(total), total, None));
            };
            let g = sftp.lock().await;
            let r = if is_dir {
                sftp::download_dir_recursive(&g, &rpath, &ltarget, &prog, &policy).await
            } else {
                sftp::download_file(&g, &rpath, &ltarget, total, &prog, &policy).await
            };
            drop(g);
            let (err, stats) = match r {
                Ok(s) => (None, Some(s)),
                Err(e) => (Some(e), None),
            };
            let _ = prog_tx.send((id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err,
                stats,
                refresh_remote: false,
                refresh_local: true,
            });
        });
    }


    /// SFTP ops: mkdir / rename / delete on remote; delete on local
    fn op_mkdir_remote(&mut self, name: String) {
        if !valid_name(&name) {
            return;
        }
        let dir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        self.rt.spawn(async move {
            let path = sftp::join_remote(&dir, &name);
            let g = sftp.lock().await;
            if let Err(e) = sftp::mkdir_all(&g, &path).await {
                let _ = tx.send(OpMsg::Error(e));
            }
        });
        self.spawn_refresh_remote(None, false);
    }

    fn op_rename_remote(&mut self, from: String, to: String) {
        if !valid_name(&to) {
            return;
        }
        let dir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        self.rt.spawn(async move {
            let a = sftp::join_remote(&dir, &from);
            let b = sftp::join_remote(&dir, &to);
            let g = sftp.lock().await;
            if let Err(e) = g.rename(&a, &b).await {
                let _ = tx.send(OpMsg::Error(e.to_string()));
            }
        });
        self.spawn_refresh_remote(None, false);
    }

    fn op_delete_remote(&mut self, name: String, dir: bool) {
        let rdir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let path = sftp::join_remote(&rdir, &name);
        self.rt.spawn(async move {
            let g = sftp.lock().await;
            let r = if dir {
                sftp::delete_recursive(&g, &path).await
            } else {
                g.remove_file(&path).await.map_err(|e| e.to_string())
            };
            if let Err(e) = r {
                let _ = tx.send(OpMsg::Error(e));
            }
        });
        self.spawn_refresh_remote(None, false);
    }

    fn op_delete_local(&mut self, name: String, dir: bool) {
        let path = self.local_dir.join(&name);
        let tx = self.op_tx.clone();
        let lang = self.lang;
        // background: removing a large tree must not block the UI thread
        self.rt.spawn_blocking(move || {
            let r = if dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match r {
                Ok(()) => {
                    let _ = tx.send(OpMsg::LocalRefresh {
                        log: Some(format!(
                            "✓ {}",
                            tpl(tr(lang, "sftp_log_deleted"), &[("name", &name)])
                        )),
                    });
                }
                Err(e) => {
                    let _ = tx.send(OpMsg::Error(e.to_string()));
                }
            }
        });
    }

    fn op_rename_local(&mut self, from: String, to: String) {
        if !valid_name(&to) {
            return;
        }
        let from_path = self.local_dir.join(&from);
        let to_path = self.local_dir.join(&to);
        if let Err(e) = std::fs::rename(&from_path, &to_path) {
            self.error = Some(e.to_string());
            self.oplog_push(format!("✗ {}", e));
        } else {
            self.oplog_push(format!(
                "✓ {}",
                tpl(tr(self.lang, "sftp_log_renamed"), &[("from", &from), ("to", &to)])
            ));
        }
        self.refresh_local();
    }

    fn op_mkdir_local(&mut self, name: String) {
        if !valid_name(&name) {
            return;
        }
        if let Err(e) = std::fs::create_dir(self.local_dir.join(&name)) {
            self.error = Some(e.to_string());
            self.oplog_push(format!("✗ {}", e));
        } else {
            self.oplog_push(format!(
                "✓ {}",
                tpl(tr(self.lang, "sftp_log_created"), &[("name", &name)])
            ));
        }
        self.refresh_local();
    }

    /// create an empty local file; auto-suffixes Explorer-style ("name (2).ext")
    /// instead of overwriting when the name is taken
    fn op_create_local_file(&mut self, name: String) {
        if !valid_name(&name) {
            return;
        }
        let path = sftp::free_local_name(&self.local_dir.join(&name));
        if let Err(e) = std::fs::File::create(&path) {
            self.error = Some(e.to_string());
            self.oplog_push(format!("✗ {}", e));
        } else {
            let actual = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            self.oplog_push(format!(
                "✓ {}",
                tpl(tr(self.lang, "sftp_log_created"), &[("name", &actual)])
            ));
        }
        self.refresh_local();
    }

    /// create an empty remote file via SFTP (CREATE|WRITE keeps existing content
    /// if the name is taken — the free-name lookup avoids that in the first place)
    fn op_create_remote_file(&mut self, name: String) {
        if !valid_name(&name) {
            return;
        }
        let dir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let start = sftp::join_remote(&dir, &name);
        self.rt.spawn(async move {
            let g = sftp.lock().await;
            let path = sftp::free_remote_name(&g, &start).await;
            let r = g
                .open_with_flags(&path, OpenFlags::CREATE | OpenFlags::WRITE)
                .await
                .map(drop)
                .map_err(|e| e.to_string());
            drop(g);
            if let Err(e) = r {
                let _ = tx.send(OpMsg::Error(e));
            }
        });
        self.spawn_refresh_remote(None, false);
    }

    // ---- context-menu actions ----

    /// open a local file/folder with the system default handler
    fn open_local(&mut self, name: String) {
        let path = self.local_dir.join(&name);
        if let Err(e) = open_with_system(&path) {
            self.error = Some(tpl(tr(self.lang, "err_open_failed"), &[("name", &name), ("e", &e.to_string())]));
        }
    }

    /// text preview of a local file
    fn view_local(&mut self, name: String) {
        let path = self.local_dir.join(&name);
        match read_text_preview(&path, &name, self.lang) {
            Ok(v) => self.viewer = Some(v),
            Err(e) => {
                self.error = Some(tpl(tr(self.lang, "err_open_failed"), &[("name", &name), ("e", &e.to_string())]));
            }
        }
    }

    /// properties of a local file/folder; folder sizes computed in the background
    fn props_local(&mut self, name: String) {
        let path = self.local_dir.join(&name);
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        self.props_id += 1;
        let id = self.props_id;
        let lang = self.lang;
        let mut rows = vec![
            (tr(lang, "col_name").to_string(), name.clone()),
            (
                tr(lang, "props_location").to_string(),
                self.local_dir.to_string_lossy().into_owned(),
            ),
            (
                tr(lang, "props_type").to_string(),
                tr(lang, if is_dir { "props_type_dir" } else { "props_type_file" }).to_string(),
            ),
        ];
        if let Ok(md) = std::fs::metadata(&path) {
            if !is_dir {
                rows.push((
                    tr(lang, "props_size").to_string(),
                    format!("{} ({})", sftp::fmt_size_pub(md.len()), md.len()),
                ));
            }
            if let Ok(t) = md.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    rows.push((
                        tr(lang, "props_modified").to_string(),
                        sftp::fmt_time_unix(d.as_secs()),
                    ));
                }
            }
        }
        if is_dir {
            self.props = Some(Props {
                id,
                title: tpl(tr(lang, "props_title"), &[("name", &name)]),
                rows,
                pending: true,
            });
            let tx = self.op_tx.clone();
            self.rt.spawn_blocking(move || {
                let (files, bytes) = sftp::count_local_bytes(&path);
                let _ = tx.send(OpMsg::PropsAppend {
                    id,
                    done: true,
                    rows: vec![
                        (tr(lang, "props_size").to_string(), format!("{} ({})", sftp::fmt_size_pub(bytes), bytes)),
                        (tr(lang, "props_files").to_string(), format!("{files}")),
                    ],
                });
            });
        } else {
            self.props = Some(Props {
                id,
                title: tpl(tr(lang, "props_title"), &[("name", &name)]),
                rows,
                pending: false,
            });
        }
    }

    /// properties of a remote file/folder via SFTP stat (+ background dir count)
    fn props_remote(&mut self, name: String) {
        let path = sftp::join_remote(&self.remote_dir, &name);
        self.props_id += 1;
        let id = self.props_id;
        let lang = self.lang;
        self.props = Some(Props {
            id,
            title: tpl(tr(lang, "props_title"), &[("name", &name)]),
            rows: vec![
                (tr(lang, "col_name").to_string(), name.clone()),
                (tr(lang, "props_location").to_string(), self.remote_dir.clone()),
            ],
            pending: true,
        });
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        self.rt.spawn(async move {
            let g = sftp.lock().await;
            match g.metadata(&path).await {
                Ok(md) => {
                    let is_dir = md.is_dir();
                    let mut rows = vec![(
                        tr(lang, "props_type").to_string(),
                        tr(lang, if is_dir { "props_type_dir" } else { "props_type_file" }).to_string(),
                    )];
                    if !is_dir {
                        let size = md.size.unwrap_or(0);
                        rows.push((
                            tr(lang, "props_size").to_string(),
                            format!("{} ({})", sftp::fmt_size_pub(size), size),
                        ));
                    }
                    if let Some(t) = md.mtime {
                        rows.push((tr(lang, "props_modified").to_string(), sftp::fmt_time_unix(t as u64)));
                    }
                    if let Some(p) = md.permissions {
                        rows.push((tr(lang, "props_permissions").to_string(), format!("0{:o}", p & 0o7777)));
                    }
                    let owner = md
                        .user
                        .clone()
                        .or_else(|| md.uid.map(|u| u.to_string()))
                        .unwrap_or_else(|| "-".into());
                    let group = md
                        .group
                        .clone()
                        .or_else(|| md.gid.map(|u| u.to_string()))
                        .unwrap_or_else(|| "-".into());
                    rows.push((tr(lang, "props_owner").to_string(), owner));
                    rows.push((tr(lang, "props_group").to_string(), group));
                    let _ = tx.send(OpMsg::PropsAppend { id, rows, done: !is_dir });
                    if is_dir {
                        let (files, bytes) = sftp::count_remote_bytes(&g, &path).await;
                        let _ = tx.send(OpMsg::PropsAppend {
                            id,
                            done: true,
                            rows: vec![
                                (
                                    tr(lang, "props_size").to_string(),
                                    format!("{} ({})", sftp::fmt_size_pub(bytes), bytes),
                                ),
                                (tr(lang, "props_files").to_string(), format!("{files}")),
                            ],
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(OpMsg::Error(e.to_string()));
                    let _ = tx.send(OpMsg::PropsAppend { id, rows: vec![], done: true });
                }
            }
        });
    }

    /// fetch a remote file into the local temp dir, then open it with the system
    /// app (`open: true`) or show the text preview (`open: false`)
    fn fetch_remote_temp(&mut self, name: String, open: bool) {
        let Some(e) = self
            .remote_entries
            .iter()
            .find(|e| e.name == name)
            .cloned()
        else {
            return;
        };
        if !open && e.size > VIEW_DL_CAP {
            let mb = (VIEW_DL_CAP / 1024 / 1024).to_string();
            self.error = Some(tpl(tr(self.lang, "view_dl_cap"), &[("mb", &mb)]));
            return;
        }
        let rpath = sftp::join_remote(&self.remote_dir, &name);
        let tmpdir = std::env::temp_dir().join(TEMP_VIEW_DIR);
        if let Err(err) = std::fs::create_dir_all(&tmpdir) {
            self.error = Some(err.to_string());
            return;
        }
        let lpath = tmpdir.join(sanitize_temp_name(&name));
        let _ = std::fs::remove_file(&lpath);
        // previous temp file may still be locked by the app it was opened with;
        // fall back to a time-prefixed name (keeps the extension for association)
        let lpath = if lpath.exists() {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            tmpdir.join(format!("{millis}-{}", sanitize_temp_name(&name)))
        } else {
            lpath
        };
        let label = if open { "sftp_open" } else { "sftp_view" };
        let id = self.start_transfer(TransferKind::Download, format!("{} [{}]", name, tr(self.lang, label)), e.size);
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let prog_tx = self.prog_tx.clone();
        let policy = sftp::ConflictPolicy::overwrite_all();
        self.rt.spawn(async move {
            let total = e.size;
            let _ = prog_tx.send((id, 0, total, None));
            let prog_tx2 = prog_tx.clone();
            let prog = move |done: u64| {
                let _ = prog_tx2.send((id, done.min(total), total, None));
            };
            let g = sftp.lock().await;
            let r = sftp::download_file(&g, &rpath, &lpath, total, &prog, &policy).await;
            drop(g);
            let (err, stats) = match r {
                Ok(s) => (None, Some(s)),
                Err(e) => (Some(e), None),
            };
            let _ = prog_tx.send((id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err: err.clone(),
                stats,
                refresh_remote: false,
                refresh_local: false,
            });
            let path = if err.is_none() { Some(lpath) } else { None };
            let _ = tx.send(OpMsg::TempReady { name, path, open, err });
        });
    }

    /// right-click menu for a local-pane row
    fn ctx_menu_local(&mut self, ui: &mut egui::Ui, i: usize, name: String, is_dir: bool) {
        let lang = self.lang;
        if ui.button(tpl(tr(lang, "sftp_upload"), &[])).clicked() {
            self.local_sel = Some(i);
            ui.close();
            self.upload_selected();
        }
        ui.separator();
        if ui.button(tpl(tr(lang, "sftp_open"), &[])).clicked() {
            ui.close();
            self.open_local(name.clone());
        }
        let view = ui.add_enabled(!is_dir, egui::Button::new(tpl(tr(lang, "sftp_view"), &[])));
        if view.clicked() {
            ui.close();
            self.view_local(name.clone());
        }
        if ui.button(tpl(tr(lang, "sftp_props"), &[])).clicked() {
            ui.close();
            self.props_local(name.clone());
        }
        ui.separator();
        if ui.button(tpl(tr(lang, "sftp_new_file"), &[])).clicked() {
            ui.close();
            self.confirm_input = String::new();
            self.confirm = Some((ConfirmKind::LocalMkfile, String::new()));
        }
        if ui.button(tpl(tr(lang, "sftp_new_dir"), &[])).clicked() {
            ui.close();
            self.confirm_input = String::new();
            self.confirm = Some((ConfirmKind::LocalMkdir, String::new()));
        }
        if ui.button(tpl(tr(lang, "sftp_rename"), &[])).clicked() {
            ui.close();
            self.confirm_input = name.clone();
            self.confirm = Some((ConfirmKind::LocalRename, name.clone()));
        }
        let del = ui.button(
            egui::RichText::new(tpl(tr(lang, "sftp_delete"), &[])).color(egui::Color32::LIGHT_RED),
        );
        if del.clicked() {
            ui.close();
            self.confirm = Some((
                if is_dir { ConfirmKind::LocalDeleteDir } else { ConfirmKind::LocalDelete },
                name,
            ));
        }
    }

    /// right-click menu for a remote-pane row
    fn ctx_menu_remote(&mut self, ui: &mut egui::Ui, i: usize, name: String, is_dir: bool) {
        let lang = self.lang;
        if ui.button(tpl(tr(lang, "sftp_download"), &[])).clicked() {
            self.remote_sel = Some(i);
            ui.close();
            self.download_selected();
        }
        ui.separator();
        let open = ui.add_enabled(!is_dir, egui::Button::new(tpl(tr(lang, "sftp_open"), &[])));
        if open.clicked() {
            ui.close();
            self.fetch_remote_temp(name.clone(), true);
        }
        let view = ui.add_enabled(!is_dir, egui::Button::new(tpl(tr(lang, "sftp_view"), &[])));
        if view.clicked() {
            ui.close();
            self.fetch_remote_temp(name.clone(), false);
        }
        if ui.button(tpl(tr(lang, "sftp_props"), &[])).clicked() {
            ui.close();
            self.props_remote(name.clone());
        }
        ui.separator();
        if ui.button(tpl(tr(lang, "sftp_new_file"), &[])).clicked() {
            ui.close();
            self.confirm_input = String::new();
            self.confirm = Some((ConfirmKind::RemoteMkfile, String::new()));
        }
        if ui.button(tpl(tr(lang, "sftp_rename"), &[])).clicked() {
            ui.close();
            self.confirm_input = name.clone();
            self.confirm = Some((ConfirmKind::RemoteRename, name.clone()));
        }
        let del = ui.button(
            egui::RichText::new(tpl(tr(lang, "sftp_delete"), &[])).color(egui::Color32::LIGHT_RED),
        );
        if del.clicked() {
            ui.close();
            self.confirm = Some((
                if is_dir { ConfirmKind::RemoteDeleteDir } else { ConfirmKind::RemoteDelete },
                name,
            ));
        }
    }



    /// Render the dual-pane browser + transfers
    /// Render the SFTP browser: remote pane (right panel) + local pane (center)
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = &self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("⚠ {err}"));
                if ui.small_button("✕").clicked() {
                    self.error = None;
                }
            });
        }
        if self.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("...");
            });
        }

        // Activity log at the bottom, spanning both panes; added BEFORE the
        // right panel so it spans the full width (panels reserve in order)
        let log_h = (ui.available_height() * 0.25).clamp(96.0, 200.0);
        egui::Panel::bottom(egui::Id::new("sftp_oplog_pane"))
            .exact_size(log_h)
            .resizable(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(tpl(tr(self.lang, "sftp_oplog"), &[])).strong());
                    if ui.small_button(tpl(tr(self.lang, "sftp_clear"), &[])).clicked() {
                        self.transfers.retain(|t| !t.finished);
                        self.oplog.clear();
                    }
                });
                // active transfers with progress bars
                if self.transfers.iter().any(|t| !t.finished) {
                    egui::ScrollArea::vertical()
                        .max_height(56.0)
                        .id_salt("sftp_oplog_transfers")
                        .show(ui, |ui| {
                            for t in self.transfers.iter().filter(|t| !t.finished) {
                                self.transfer_row(ui, t);
                            }
                        });
                }
                // finished transfers + operation lines, newest at the bottom
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .id_salt("sftp_oplog_lines")
                    .show(ui, |ui| {
                        for t in self.transfers.iter().filter(|t| t.finished) {
                            self.transfer_row(ui, t);
                        }
                        for line in &self.oplog {
                            ui.label(egui::RichText::new(line).monospace().small());
                        }
                    });
            });

        // Remote pane on the right: a real panel so both panes always fit
        // exact 50/50 split, re-applied every frame (persisted panel state
        // would otherwise override the default)
        let half = ui.available_width() * 0.5;
        egui::Panel::right(egui::Id::new("sftp_remote_pane"))
            .exact_size(half)
            .resizable(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(tpl(tr(self.lang, "sftp_remote"), &[])).strong());
                    let r_go = ui.add(
                        egui::TextEdit::singleline(&mut self.remote_edit).desired_width(160.0),
                    );
                    let enter = r_go.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button(tpl(tr(self.lang, "sftp_go"), &[])).clicked() || enter {
                        let d = self.remote_edit.clone();
                        self.spawn_refresh_remote(Some(&d), false);
                    }
                    if ui.button(tpl(tr(self.lang, "sftp_up"), &[])).clicked() {
                        let up = sftp::join_remote(&self.remote_dir, "..");
                        self.spawn_refresh_remote(Some(&up), true);
                    }
                    if ui.button(tpl(tr(self.lang, "sftp_refresh"), &[])).clicked() {
                        self.spawn_refresh_remote(None, false);
                    }
                });
                ui.separator();

                // remote ops
                ui.horizontal(|ui| {
                    if ui.button(tpl(tr(self.lang, "sftp_new_file"), &[])).clicked() {
                        self.confirm_input = String::new();
                        self.confirm = Some((ConfirmKind::RemoteMkfile, String::new()));
                    }
                    if ui.button(tpl(tr(self.lang, "sftp_new_dir"), &[])).clicked() {
                        self.confirm_input = String::new();
                        self.confirm = Some((ConfirmKind::RemoteMkdir, String::new()));
                    }
                    let rn = ui.add_enabled(
                        self.remote_sel.is_some(),
                        egui::Button::new(tpl(tr(self.lang, "sftp_rename"), &[])),
                    );
                    if rn.clicked() {
                        if let Some(i) = self.remote_sel {
                            if let Some(e) = self.remote_entries.get(i) {
                                self.confirm_input = e.name.clone();
                                self.confirm = Some((ConfirmKind::RemoteRename, e.name.clone()));
                            }
                        }
                    }
                    let del = ui.add_enabled(
                        self.remote_sel.is_some(),
                        egui::Button::new(egui::RichText::new(tpl(tr(self.lang, "sftp_delete"), &[])).color(egui::Color32::LIGHT_RED)),
                    );
                    if del.clicked() {
                        if let Some(i) = self.remote_sel {
                            if let Some(e) = self.remote_entries.get(i) {
                                self.confirm = Some((
                                    if e.is_dir { ConfirmKind::RemoteDeleteDir } else { ConfirmKind::RemoteDelete },
                                    e.name.clone(),
                                ));
                            }
                        } else if let Some(i) = self.local_sel {
                            if let Some(e) = self.local_entries.get(i) {
                                self.confirm = Some((
                                    if e.is_dir { ConfirmKind::LocalDeleteDir } else { ConfirmKind::LocalDelete },
                                    e.name.clone(),
                                ));
                            }
                        }
                    }
                    if self.loading {
                        ui.spinner();
                    }
                });
                ui.separator();
                self.sort_bar(ui, Pane::Remote);
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .id_salt("sftp_remote_list")
                    .show(ui, |ui| {
                        for (i, e) in self.remote_entries.clone().iter().enumerate() {
                            let label = if e.is_dir {
                                egui::RichText::new(format!("{}{}", e.name, "/")).strong()
                            } else {
                                egui::RichText::new(format!("{}  ({})", e.name, crate::sftp::fmt_size_pub(e.size))).weak()
                            };
                            let resp = ui.selectable_label(self.remote_sel == Some(i), label);
                            if resp.clicked() || resp.secondary_clicked() {
                                self.remote_sel = Some(i);
                            }
                            if resp.double_clicked() && e.is_dir {
                                let d = sftp::join_remote(&self.remote_dir, &e.name);
                                self.spawn_refresh_remote(Some(&d), false);
                                self.remote_sel = None;
                            }
                            resp.context_menu(|ui| {
                                self.ctx_menu_remote(ui, i, e.name.clone(), e.is_dir);
                            });
                        }
                    });

                // OS drag&drop hover feedback (drawn last = on top of the list)
                if !ui.input(|i| i.raw.hovered_files.is_empty()) {
                    let rect = ui.max_rect();
                    let green = egui::Color32::from_rgb(74, 246, 118);
                    ui.painter().rect_filled(
                        rect,
                        8.0,
                        egui::Color32::from_rgba_unmultiplied(74, 246, 118, 20),
                    );
                    ui.painter().rect_stroke(rect, 8.0, (2.5, green), egui::StrokeKind::Inside);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        tpl(tr(self.lang, "sftp_drop_hint"), &[]),
                        egui::FontId::proportional(20.0),
                        green,
                    );
                }
            });

        // Local pane + transfers in the center
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(tpl(tr(self.lang, "sftp_local"), &[])).strong());
                let r_go = ui.add(
                    egui::TextEdit::singleline(&mut self.local_edit).desired_width(200.0),
                );
                let enter = r_go.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button(tpl(tr(self.lang, "sftp_go"), &[])).clicked() || enter {
                    let d = PathBuf::from(self.local_edit.trim());
                    if d.is_dir() {
                        self.local_dir = d.clone();
                        self.refresh_local();
                    } else {
                        let msg = format!("{} (not a directory)", d.display());
                        self.error = Some(tpl(tr(self.lang, "err_open_failed"), &[("e", &msg)]));
                    }
                }
                if ui.button(tpl(tr(self.lang, "sftp_up"), &[])).clicked() {
                    if let Some(p) = self.local_dir.parent() {
                        self.local_dir = p.to_path_buf();
                        self.refresh_local();
                    }
                }
                if ui.button(tpl(tr(self.lang, "sftp_refresh"), &[])).clicked() {
                    self.refresh_local();
                }
            });
            ui.separator();

            // local ops
            ui.horizontal(|ui| {
                if ui.button(tpl(tr(self.lang, "sftp_new_file"), &[])).clicked() {
                    self.confirm_input = String::new();
                    self.confirm = Some((ConfirmKind::LocalMkfile, String::new()));
                }
                if ui.button(tpl(tr(self.lang, "sftp_new_dir"), &[])).clicked() {
                    self.confirm_input = String::new();
                    self.confirm = Some((ConfirmKind::LocalMkdir, String::new()));
                }
            });
            ui.separator();

            // transfer buttons
            ui.horizontal(|ui| {
                let dl = ui.add_enabled(
                    self.remote_sel.is_some(),
                    egui::Button::new(tpl(tr(self.lang, "sftp_download"), &[])),
                );
                if dl.clicked() {
                    self.download_selected();
                }
                let ul = ui.add_enabled(
                    self.local_sel.is_some(),
                    egui::Button::new(tpl(tr(self.lang, "sftp_upload"), &[])),
                );
                if ul.clicked() {
                    self.upload_selected();
                }
            });
            ui.separator();
            self.sort_bar(ui, Pane::Local);
            ui.separator();

            ui.label(egui::RichText::new(self.local_dir.to_string_lossy().as_ref()).weak().small());
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .id_salt("sftp_local_list")
                .show(ui, |ui| {
                    for (i, e) in self.local_entries.clone().iter().enumerate() {
                        let label = if e.is_dir {
                            egui::RichText::new(format!("{}{}", e.name, "/")).strong()
                        } else {
                            egui::RichText::new(format!("{}  ({})", e.name, crate::sftp::fmt_size_pub(e.size))).weak()
                        };
                        let resp = ui.selectable_label(self.local_sel == Some(i), label);
                        if resp.clicked() || resp.secondary_clicked() {
                            self.local_sel = Some(i);
                        }
                        if resp.double_clicked() && e.is_dir {
                            self.local_dir = self.local_dir.join(&e.name);
                            self.refresh_local();
                            self.local_sel = None;
                        }
                        resp.context_menu(|ui| {
                            self.ctx_menu_local(ui, i, e.name.clone(), e.is_dir);
                        });
                    }
                });
        });

        // confirm / prompt windows
        self.confirm_ui(ui);
        self.transfer_ask_ui(ui);
        self.viewer_ui(ui);
        self.props_ui(ui);
    }

    /// one transfers-list row (progress bar while running; ✓/⚠ result after)
    fn transfer_row(&self, ui: &mut egui::Ui, t: &Transfer) {
        ui.horizontal(|ui| {
            let (icon, color) = if let Some(_e) = &t.err {
                ("⚠", egui::Color32::LIGHT_RED)
            } else if t.finished {
                ("✓", egui::Color32::LIGHT_GREEN)
            } else {
                (
                    match t.kind {
                        TransferKind::Upload => "⬆",
                        TransferKind::Download => "⬇",
                    },
                    egui::Color32::LIGHT_GREEN,
                )
            };
            ui.label(egui::RichText::new(icon).color(color));
            let frac = if t.total > 0 { t.done as f32 / t.total as f32 } else { 1.0 };
            ui.add(
                egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                    .show_percentage()
                    .desired_height(14.0),
            );
            ui.label(
                egui::RichText::new(format!(
                    "{} {}/{}",
                    t.name,
                    crate::sftp::fmt_size_pub(t.done),
                    crate::sftp::fmt_size_pub(t.total)
                ))
                .weak()
                .small(),
            );
        });
    }

    /// transfer conflict dialog: the asking transfer task is blocked on the answer,
    /// so the dialog must be answered (no close button); queued conflicts follow one by one
    fn transfer_ask_ui(&mut self, ui: &mut egui::Ui) {
        let Some((dest, resp)) = self.transfer_ask.take() else { return };
        let lang = self.lang;
        let mut choice: Option<u8> = None;
        egui::Window::new(tpl(tr(lang, "sftp_conflict_title"), &[]))
            .id(egui::Id::new("sftp_conflict_win"))
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
            None => self.transfer_ask = Some((dest, resp)),
        }
    }

    /// quick-view text window
    fn viewer_ui(&mut self, ui: &mut egui::Ui) {        let Some(mut v) = self.viewer.take() else { return };
        let lang = self.lang;
        let mut open = true;
        egui::Window::new(v.title.clone())
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(720.0)
            .default_height(480.0)
            .show(ui, |ui| {
                if let Some(note) = &v.note {
                    ui.label(egui::RichText::new(note).weak().small());
                }
                ui.horizontal(|ui| {
                    ui.checkbox(&mut v.wrap, tpl(tr(lang, "view_wrap"), &[]));
                });
                egui::ScrollArea::new([!v.wrap, true])
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if v.wrap {
                            ui.add(egui::Label::new(egui::RichText::new(&v.text).monospace()).wrap());
                        } else {
                            ui.monospace(&v.text);
                        }
                    });
            });
        if open {
            self.viewer = Some(v);
        }
    }

    /// properties window
    fn props_ui(&mut self, ui: &mut egui::Ui) {
        let Some(p) = self.props.clone() else { return };
        let lang = self.lang;
        let mut open = true;
        egui::Window::new(p.title)
            .id(egui::Id::new("sftp_props_win"))
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .resizable(false)
            .collapsible(false)
            .show(ui, |ui| {
                let rows_len = self.props.as_ref().map(|p| p.rows.len()).unwrap_or(0);
                egui::Grid::new("sftp_props_grid")
                    .num_columns(2)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for i in 0..rows_len {
                            if let Some(p) = self.props.as_ref() {
                                let (k, v) = &p.rows[i];
                                ui.strong(k);
                                ui.label(v);
                                ui.end_row();
                            }
                        }
                    });
                if self.props.as_ref().map(|p| p.pending).unwrap_or(false) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tpl(tr(lang, "props_computing"), &[]));
                    });
                }
            });
        if !open {
            self.props = None;
        }
    }




    /// confirm / prompt windows for SFTP ops
    pub fn confirm_ui(&mut self, ui: &mut egui::Ui) {
        let Some((kind, target)) = self.confirm.clone() else { return };
        let lang = self.lang;
        let (title, prompt, with_input) = match kind {
            ConfirmKind::RemoteDelete => (
                tpl(tr(lang, "sftp_delete"), &[]),
                tpl(tr(lang, "sftp_confirm_delete"), &[("name", &target)]),
                false,
            ),
            ConfirmKind::RemoteDeleteDir => (
                tpl(tr(lang, "sftp_delete"), &[]),
                tpl(tr(lang, "sftp_confirm_delete_dir"), &[("name", &target)]),
                false,
            ),
            ConfirmKind::LocalDelete => (
                tpl(tr(lang, "sftp_delete"), &[]),
                tpl(tr(lang, "sftp_confirm_delete"), &[("name", &target)]),
                false,
            ),
            ConfirmKind::LocalDeleteDir => (
                tpl(tr(lang, "sftp_delete"), &[]),
                tpl(tr(lang, "sftp_confirm_delete_dir"), &[("name", &target)]),
                false,
            ),
            ConfirmKind::RemoteRename => (
                tpl(tr(lang, "sftp_rename"), &[]),
                tpl(tr(lang, "sftp_new_name"), &[]),
                true,
            ),
            ConfirmKind::RemoteMkdir => (
                tpl(tr(lang, "sftp_new_dir"), &[]),
                tpl(tr(lang, "sftp_new_name"), &[]),
                true,
            ),
            ConfirmKind::LocalRename => (
                tpl(tr(lang, "sftp_rename"), &[]),
                tpl(tr(lang, "sftp_new_name"), &[]),
                true,
            ),
            ConfirmKind::LocalMkdir | ConfirmKind::LocalMkfile => (
                tpl(
                    tr(lang, if kind == ConfirmKind::LocalMkdir { "sftp_new_dir" } else { "sftp_new_file" }),
                    &[],
                ),
                tpl(tr(lang, "sftp_new_name"), &[]),
                true,
            ),
            ConfirmKind::RemoteMkfile => (
                tpl(tr(lang, "sftp_new_file"), &[]),
                tpl(tr(lang, "sftp_new_name"), &[]),
                true,
            ),
        };
        let mut open = true;
        let mut ok = false;
        let mut cancel = false;
        egui::Window::new(title)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ui, |ui| {
                ui.label(prompt);
                if with_input {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.confirm_input).desired_width(260.0),
                    );
                    if !self.confirm_input_focus_done {
                        r.request_focus();
                        self.confirm_input_focus_done = true;
                    }
                    // Enter in the field confirms (singleline loses focus on Enter)
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        ok = true;
                    }
                } else if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // delete confirmations: Enter = OK
                    ok = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(tr(lang, "btn_ok")).clicked() {
                        ok = true;
                    }
                    if ui.button(tr(lang, "btn_cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel || !open {
            self.confirm = None;
            self.confirm_input_focus_done = false;
        } else if ok {
            let name = target.clone();
            let input = self.confirm_input.clone();
            self.confirm = None;
            self.confirm_input_focus_done = false;
            match kind {
                ConfirmKind::RemoteDelete => self.op_delete_remote(name, false),
                ConfirmKind::RemoteDeleteDir => self.op_delete_remote(name, true),
                ConfirmKind::LocalDelete => self.op_delete_local(name, false),
                ConfirmKind::LocalDeleteDir => self.op_delete_local(name, true),
                ConfirmKind::RemoteRename => self.op_rename_remote(name, input),
                ConfirmKind::RemoteMkdir => self.op_mkdir_remote(input),
                ConfirmKind::RemoteMkfile => self.op_create_remote_file(input),
                ConfirmKind::LocalRename => self.op_rename_local(name, input),
                ConfirmKind::LocalMkdir => self.op_mkdir_local(input),
                ConfirmKind::LocalMkfile => self.op_create_local_file(input),
            }
        }
    }

    /// best-effort cleanup when the tab closes: drop fetched temp preview files
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(TEMP_VIEW_DIR));
    }
}

/// reject empty names and anything containing path separators / Windows-illegal chars
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.chars().any(|c| {
            matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

/// open a path with the OS default handler (file: associated app; folder: Explorer).
/// Uses explorer.exe directly on Windows — `cmd /C start` would let cmd metacharacters
/// in (remote) file names be interpreted as command separators.
fn open_with_system(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
        Ok(())
    }
}

/// replace characters that Windows file names forbid (remote names may contain them)
fn sanitize_temp_name(name: &str) -> String {
    name.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect()
}

/// read a local file into a quick-view Viewer (text only, capped, binary-detected)
fn read_text_preview(path: &std::path::Path, name: &str, lang: Language) -> std::io::Result<Viewer> {
    let total = std::fs::metadata(path)?.len();
    let cap = total.min(VIEW_TEXT_CAP);
    let f = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(cap as usize);
    (&f).take(cap).read_to_end(&mut bytes)?;
    let binary = bytes[..bytes.len().min(8192)].contains(&0);
    let note = if binary {
        Some(tpl(
            tr(lang, "view_binary"),
            &[("size", &sftp::fmt_size_pub(total))],
        ))
    } else {
        (total > VIEW_TEXT_CAP).then(|| {
            tpl(
                tr(lang, "view_truncated"),
                &[
                    ("cap", &sftp::fmt_size_pub(VIEW_TEXT_CAP)),
                    ("total", &sftp::fmt_size_pub(total)),
                ],
            )
        })
    };
    Ok(Viewer {
        title: tpl(tr(lang, "view_title"), &[("name", name)]),
        text: if binary { String::new() } else { String::from_utf8_lossy(&bytes).into_owned() },
        note,
        wrap: false,
    })
}

