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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmKind {
    RemoteDelete,
    RemoteDeleteDir,
    LocalDelete,
    LocalDeleteDir,
    RemoteRename,
    RemoteMkdir,
}

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
        egui::Panel::right(egui::Id::new("sftp_remote_pane"))
            .resizable(false)
            .show_inside(ui, |ui| {
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
                            if resp.clicked() {
                                self.remote_sel = Some(i);
                            }
                            if resp.double_clicked() && e.is_dir {
                                let d = sftp::join_remote(&self.remote_dir, &e.name);
                                self.spawn_refresh_remote(Some(&d), false);
                                self.remote_sel = None;
                            }
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
                        if resp.clicked() {
                            self.local_sel = Some(i);
                        }
                        if resp.double_clicked() && e.is_dir {
                            self.local_dir = self.local_dir.join(&e.name);
                            self.refresh_local();
                            self.local_sel = None;
                        }
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
            let kind = kind;
            self.confirm = None;
            self.confirm_input_focus_done = false;
            match kind {
                ConfirmKind::RemoteDelete => self.op_delete_remote(name, false),
                ConfirmKind::RemoteDeleteDir => self.op_delete_remote(name, true),
                ConfirmKind::LocalDelete => self.op_delete_local(name, false),
                ConfirmKind::LocalDeleteDir => self.op_delete_local(name, true),
                ConfirmKind::RemoteRename => self.op_rename_remote(name, input),
                ConfirmKind::RemoteMkdir => self.op_mkdir_remote(input),
            }
        }
    }
}

