use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use russh_sftp::client::SftpSession;
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
}

pub enum OpMsg {
    RemoteList { dir: String, entries: Vec<FileEntry> },
    Error(String),
    TransferDone { id: u64, err: Option<String>, refresh_remote: bool, refresh_local: bool },
    RemoteDirSet(String),
    /// append rows to the properties window with matching id (async dir-size results)
    PropsAppend { id: u64, rows: Vec<(String, String)> },
    /// remote file fetched into a temp file: open with system app or in the text viewer
    TempReady { name: String, path: Option<PathBuf>, open: bool, err: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmKind {
    RemoteDelete,
    RemoteDeleteDir,
    LocalDelete,
    LocalDeleteDir,
    RemoteRename,
    RemoteMkdir,
    LocalRename,
    LocalMkdir,
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

    pub local_dir: PathBuf,
    pub local_entries: Vec<FileEntry>,
    pub local_sel: Option<usize>,
    pub local_edit: String,

    pub remote_dir: String,
    pub remote_entries: Vec<FileEntry>,
    pub remote_sel: Option<usize>,
    pub remote_edit: String,

    pub loading: bool,
    pub error: Option<String>,
    op_rx: mpsc::UnboundedReceiver<OpMsg>,
    op_tx: mpsc::UnboundedSender<OpMsg>,

    pub transfers: Vec<Transfer>,
    prog_rx: mpsc::UnboundedReceiver<(u64, u64, u64, Option<String>)>,
    prog_tx: mpsc::UnboundedSender<(u64, u64, u64, Option<String>)>,
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
    ) -> Self {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let (prog_tx, prog_rx) = mpsc::unbounded_channel();
        let mut st = Self {
            name,
            sftp: sftp.clone(),
            close_tx,
            rt,
            lang,
            local_dir: local_root.clone(),
            local_entries: Vec::new(),
            local_sel: None,
            local_edit: local_root.to_string_lossy().into_owned(),
            remote_dir: String::from("/"),
            remote_entries: Vec::new(),
            remote_sel: None,
            remote_edit: String::new(),
            loading: true,
            error: None,
            op_rx,
            op_tx,
            transfers: Vec::new(),
            prog_rx,
            prog_tx,
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
        let mut entries: Vec<FileEntry> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let Ok(md) = e.metadata() else { continue };
                entries.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: ft.is_dir(),
                    size: md.len(),
                    mtime: md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as u32)
                        .unwrap_or(0),
                });
            }
        }
        sftp::sort_entries(&mut entries);
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
            if resolve_cwd {
                if let Ok(cwd) = sftp.lock().await.canonicalize(".").await {
                    let _ = tx.send(OpMsg::RemoteDirSet(cwd));
                }
            }
            let guard = sftp.lock().await;
            match crate::sftp::list_dir(&guard, &start).await {
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
                    self.remote_edit = dir.clone();
                    self.remote_dir = dir;
                    self.remote_entries = entries;
                    self.remote_sel = None;
                    self.loading = false;
                    refresh_remote = false;
                }
                OpMsg::Error(e) => {
                    self.error = Some(e);
                    self.loading = false;
                }
                OpMsg::RemoteDirSet(d) => {
                    self.remote_edit = d.clone();
                    self.remote_dir = d;
                }
                OpMsg::TransferDone { id, err, refresh_remote: rr, refresh_local: rl } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.err = err;
                        t.finished = true;
                    }
                    refresh_remote |= rr;
                    refresh_local |= rl;
                }
                OpMsg::PropsAppend { id, rows } => {
                    if let Some(p) = &mut self.props {
                        if p.id == id {
                            p.rows.extend(rows);
                            p.pending = false;
                        }
                    }
                }
                OpMsg::TempReady { name, path, open, err } => {
                    if let Some(e) = err {
                        self.error = Some(tpl(tr(self.lang, "err_open_failed"), &[("name", &name), ("e", &e)]));
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
        });
        id
    }



    /// Upload selected local file/dir into the current remote dir
    fn upload_selected(&mut self) {
        let Some(idx) = self.local_sel else { return };
        let Some(e) = self.local_entries.get(idx) else { return };
        let lname = e.name.clone();
        let is_dir = e.is_dir;
        let lpath = self.local_dir.join(&lname);
        let rdir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let id = self.start_transfer(
            TransferKind::Upload,
            lname.clone(),
            if is_dir { 0 } else { e.size },
        );
        let prog_id = id;
        let prog_tx = self.prog_tx.clone();
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
                sftp::upload_dir_recursive(&g, &lpath, &rdir, &prog).await
            } else {
                sftp::upload_file(&g, &lpath, &rdir, total, &prog).await
            };
            drop(g);
            let err = r.err();
            let _ = prog_tx.send((prog_id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err,
                refresh_remote: true,
                refresh_local: false,
            });
        });
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
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let id = self.start_transfer(
            TransferKind::Download,
            rname.clone(),
            if is_dir { 0 } else { size },
        );
        let prog_tx = self.prog_tx.clone();
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
                sftp::download_dir_recursive(&g, &rpath, &ldir, &prog).await
            } else {
                sftp::download_file(&g, &rpath, &ldir, total, &prog).await
            };
            drop(g);
            let err = r.err();
            let _ = prog_tx.send((id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err,
                refresh_remote: false,
                refresh_local: true,
            });
        });
    }


    /// SFTP ops: mkdir / rename / delete on remote; delete on local
    fn op_mkdir_remote(&mut self, name: String) {
        let dir = self.remote_dir.clone();
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        self.busy = true;
        self.rt.spawn(async move {
            let path = sftp::join_remote(&dir, &name);
            let g = sftp.lock().await;
            if let Err(e) = sftp::mkdir_all(&g, &path).await {
                let _ = tx.send(OpMsg::Error(e));
            }
            drop(g);
            let _ = tx.send(OpMsg::RemoteList { dir, entries: Vec::new() });
        });
        self.spawn_refresh_remote(None, false);
    }

    fn op_rename_remote(&mut self, from: String, to: String) {
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
        let r = if dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = r {
            self.error = Some(e.to_string());
        }
        self.refresh_local();
    }

    fn op_rename_local(&mut self, from: String, to: String) {
        let from = self.local_dir.join(&from);
        let to = self.local_dir.join(&to);
        if let Err(e) = std::fs::rename(&from, &to) {
            self.error = Some(e.to_string());
        }
        self.refresh_local();
    }

    fn op_mkdir_local(&mut self, name: String) {
        if let Err(e) = std::fs::create_dir(self.local_dir.join(&name)) {
            self.error = Some(e.to_string());
        }
        self.refresh_local();
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
                    let _ = tx.send(OpMsg::PropsAppend { id, rows });
                    if is_dir {
                        let (files, bytes) = sftp::count_remote_bytes(&g, &path).await;
                        let _ = tx.send(OpMsg::PropsAppend {
                            id,
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
                    let _ = tx.send(OpMsg::PropsAppend { id, rows: vec![] });
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
            self.error = Some(tpl(tr(self.lang, "view_dl_cap"), &[("mb", "16")]));
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
        let label = if open { "sftp_open" } else { "sftp_view" };
        let id = self.start_transfer(TransferKind::Download, format!("{} [{}]", name, tr(self.lang, label)), e.size);
        let sftp = self.sftp.clone();
        let tx = self.op_tx.clone();
        let prog_tx = self.prog_tx.clone();
        self.rt.spawn(async move {
            let total = e.size;
            let _ = prog_tx.send((id, 0, total, None));
            let prog_tx2 = prog_tx.clone();
            let prog = move |done: u64| {
                let _ = prog_tx2.send((id, done.min(total), total, None));
            };
            let g = sftp.lock().await;
            let r = sftp::download_file(&g, &rpath, &lpath, total, &prog).await;
            drop(g);
            let err = r.err();
            let _ = prog_tx.send((id, total, total, err.clone()));
            let _ = tx.send(OpMsg::TransferDone {
                id,
                err: err.clone(),
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
                        self.error = Some(d.to_string_lossy().into_owned());
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

            // transfers
            if !self.transfers.is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(tpl(tr(self.lang, "sftp_transfers"), &[])).strong());
                    if ui.small_button(tpl(tr(self.lang, "sftp_clear"), &[])).clicked() {
                        self.transfers.retain(|t| !t.finished);
                    }
                });
                egui::ScrollArea::vertical()
                    .max_height(96.0)
                    .id_salt("sftp_transfers")
                    .show(ui, |ui| {
                        for t in &self.transfers {
                            ui.horizontal(|ui| {
                                let icon = if t.err.is_some() {
                                    "⚠"
                                } else {
                                    match t.kind {
                                        TransferKind::Upload => "⬆",
                                        TransferKind::Download => "⬇",
                                    }
                                };
                                ui.label(egui::RichText::new(icon).color(if t.err.is_some() {
                                    egui::Color32::LIGHT_RED
                                } else {
                                    egui::Color32::LIGHT_GREEN
                                }));
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
                    });
            }
        });

        // confirm / prompt windows
        self.confirm_ui(ui);
        self.viewer_ui(ui);
        self.props_ui(ui);
    }

    /// quick-view text window
    fn viewer_ui(&mut self, ui: &mut egui::Ui) {
        let Some(mut v) = self.viewer.take() else { return };
        let lang = self.lang;
        let mut open = true;
        egui::Window::new(v.title.clone())
            .open(&mut open)
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
            ConfirmKind::LocalMkdir => (
                tpl(tr(lang, "sftp_new_dir"), &[]),
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
                    if self.confirm_input.is_empty() && !self.confirm_input_focus_done {
                        r.request_focus();
                        self.confirm_input_focus_done = true;
                    }
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
                ConfirmKind::LocalRename => self.op_rename_local(name, input),
                ConfirmKind::LocalMkdir => self.op_mkdir_local(input),
            }
        }
    }
}

/// open a path with the OS default handler (file: associated app; folder: Explorer)
fn open_with_system(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
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

