//! Main application UI: server list sidebar + terminal tabs + dialogs.
//!
//! One `Tab` per connection. Connection tasks live in the background tokio
//! runtime; the UI polls their channels every frame (connect results, connect-time
//! questions like password prompts / host-key confirms, PTY output, close events).

use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};

use crate::fonts;
use crate::gconfig::{BellMode, GuiConfig, Theme};
use crate::i18n::{tpl, tr, Language};
use crate::local::{self, ShellKind};
use crate::sftpui::SftpTab;
use crate::session::{self, ConnectError, ConnectOpts, ConnectRequest, SessionEvent, SessionHandle};
use crate::term::Terminal;
use crate::xconfig::{load_settings, save_settings, AppSettings, AuthMethod, Server};

/// Effective connection options: per-server proxy > global proxy (same merge as xxssh)
fn resolve_opts(server: &Server, settings: &AppSettings) -> ConnectOpts {
    let proxy = if !server.proxy.trim().is_empty() {
        Some(server.proxy.trim().to_string())
    } else if settings.enable_proxy && !settings.global_proxy.trim().is_empty() {
        Some(settings.global_proxy.trim().to_string())
    } else {
        None
    };
    ConnectOpts {
        proxy,
        connect_timeout: None,
        known_hosts: None,
        known_hosts_add: None,
        // Persistent TOFU store: trust once, later connects skip the dialog
        host_keystore: dirs::home_dir()
            .map(|h| h.join(".xxssh").join("known_hosts").to_string_lossy().into_owned()),
    }
}

/// A terminal tab
enum Tab {
    Connecting {
        name: String,
        result_rx: oneshot::Receiver<Result<SessionHandle, ConnectError>>,
        request_rx: mpsc::UnboundedReceiver<ConnectRequest>,
        status: String,
        /// kept so a closed session tab can offer one-click reconnect
        server: Server,
    },
    Open {
        name: String,
        term: Terminal,
        title_rx: mpsc::UnboundedReceiver<String>,
        bell_rx: mpsc::UnboundedReceiver<()>,
        handle: SessionHandle,
        /// PTY size sent at least once (grid size starts at 80x24 until first paint)
        resize_sent: bool,
        closed: Option<String>,
        bell_flash: bool,
        /// None for local shells (no reconnect possible)
        server: Option<Server>,
    },
    Failed {
        name: String,
        error: String,
        /// kept so the tab menu can offer reconnect
        server: Option<Server>,
    },
    SftpConnecting {
        name: String,
        result_rx: oneshot::Receiver<Result<crate::session::SftpClient, ConnectError>>,
        request_rx: mpsc::UnboundedReceiver<ConnectRequest>,
        status: String,
        server: Server,
    },
    Sftp {
        st: Box<SftpTab>,
    },
}

impl Tab {
    fn name(&self) -> &str {
        match self {
            Tab::Connecting { name, .. }
            | Tab::Open { name, .. }
            | Tab::Failed { name, .. } => name,
            Tab::Sftp { st } => &st.name,
            Tab::SftpConnecting { name, .. } => name,
        }
    }

    /// server this tab belongs to, when a reconnect makes sense
    /// (local shells have none; already-connecting tabs don't need one)
    fn reconnect_server(&self) -> Option<&Server> {
        match self {
            Tab::Open { server: Some(s), .. } => Some(s),
            Tab::Failed { server: Some(s), .. } => Some(s),
            Tab::Sftp { st } => Some(&st.server),
            _ => None,
        }
    }
}

/// Server add/edit form state
struct ServerForm {
    editing: Option<usize>, // index into servers when editing
    name: String,
    host: String,
    port: String,
    username: String,
    auth: AuthMethod,
    password: String,
    key_path: String,
    key_passphrase: String,
    proxy: String,
    error: Option<String>,
}

impl ServerForm {
    fn new() -> Self {
        Self {
            editing: None,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: "root".into(),
            auth: AuthMethod::Password,
            password: String::new(),
            key_path: String::new(),
            key_passphrase: String::new(),
            proxy: String::new(),
            error: None,
        }
    }

    fn from_server(idx: usize, s: &Server) -> Self {
        Self {
            editing: Some(idx),
            name: s.name.clone(),
            host: s.host.clone(),
            port: s.port.to_string(),
            username: s.username.clone(),
            auth: s.auth,
            password: s.password.clone(),
            key_path: s.key_path.clone(),
            key_passphrase: s.key_passphrase.clone(),
            proxy: s.proxy.clone(),
            error: None,
        }
    }

    fn build(&self) -> Server {
        Server {
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth: self.auth,
            password: self.password.clone(),
            key_path: self.key_path.trim().to_string(),
            key_passphrase: self.key_passphrase.clone(),
            proxy: self.proxy.trim().to_string(),
        }
    }
}

/// Quick Connect dialog state: connect without saving to servers.json
struct QuickForm {
    host: String,
    port: String,
    username: String,
    password: String,
    error: Option<String>,
    /// request focus for the host field on the first frame
    focus_host: bool,
}

impl QuickForm {
    fn from_gcfg(g: &GuiConfig) -> Self {
        Self {
            host: g.quick_host.clone(),
            port: g.quick_port.to_string(),
            username: if g.quick_user.is_empty() { "root".into() } else { g.quick_user.clone() },
            password: String::new(),
            error: None,
            focus_host: true,
        }
    }

    fn build(&self) -> Server {
        let host = self.host.trim().to_string();
        Server {
            name: format!("{host}:{}", self.port.trim()),
            host,
            port: self.port.trim().parse().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth: AuthMethod::Password,
            password: self.password.clone(),
            key_path: String::new(),
            key_passphrase: String::new(),
            proxy: String::new(),
        }
    }
}

/// A pending connect-time question awaiting an answer (one dialog at a time)
struct PendingQuestion {
    question: Question,
}

enum Question {
    Password {
        user: String,
        host: String,
        respond: Option<oneshot::Sender<Option<String>>>,
        input: String,
    },
    Passphrase {
        path: String,
        respond: Option<oneshot::Sender<Option<String>>>,
        input: String,
    },
    HostKey {
        host: String,
        port: u16,
        fingerprint: String,
        changed: bool,
        respond: Option<oneshot::Sender<bool>>,
    },
}

/// Self-update dialog state machine (driven by messages from worker threads)
enum UpdateUi {
    Closed,
    Checking,
    UpToDate { version: String },
    Available { latest: String, url: String, sha256: String },
    Downloading { latest: String, done: u64, total: u64 },
    Done { version: String },
    Failed { msg: String },
}

enum UpdateMsg {
    Checked(Result<crate::update::UpdateCheck, String>),
    Progress(u64, u64),
    Installed(Result<(), String>),
}

enum UpdateAction {
    Check,
    Start,
    Restart,
    Cancel,
}

pub struct XxsshgApp {
    rt: tokio::runtime::Handle,
    /// Terminal monospace fonts available on this machine (labels)
    mono_fonts: Vec<&'static str>,
    /// Last painted terminal grid size — new sessions start at this size so the
    /// initial PTY matches the window (avoids a reflow that shifts the cursor)
    last_grid: (u16, u16),
    pub servers_path: PathBuf,
    pub settings_path: PathBuf,
    pub gui_path: PathBuf,
    pub servers: Vec<Server>,
    pub settings: AppSettings,
    pub gcfg: GuiConfig,

    tabs: Vec<Tab>,
    active_tab: usize,
    selected_server: usize,

    form: Option<ServerForm>,
    form_open: bool,
    quick: Option<QuickForm>,
    quick_open: bool,
    delete_confirm: Option<usize>,
    question: Option<PendingQuestion>,
    settings_open: bool,
    about_open: bool,
    /// previous active tab (to auto-focus newly activated terminals)
    prev_active_tab: usize,
    /// server list sort by name: Some(true)=asc, Some(false)=desc,
    /// None = natural order (the order persisted in servers.json)
    server_sort: Option<bool>,
    /// self-update dialog
    update_ui: UpdateUi,
    update_rx: Option<std::sync::mpsc::Receiver<UpdateMsg>>,
    quit_confirm: bool,
    status_msg: Option<(String, std::time::Instant)>,
    autoconnect_done: bool,
    /// Tab close / new-cmd requested by a terminal hotkey (handled next frame)
    deferred_close: Option<usize>,
    deferred_new_cmd: bool,
    /// hotkey key-state edge detectors (true = currently held)
    hk_close_down: bool,
    hk_new_down: bool,
}

impl XxsshgApp {
    pub fn new(
        rt: tokio::runtime::Handle,
        servers_path: PathBuf,
        settings_path: PathBuf,
        gui_path: PathBuf,
        servers: Vec<Server>,
        settings: AppSettings,
        gcfg: GuiConfig,
    ) -> Self {
        Self {
            rt,
            mono_fonts: fonts::available_monos(),
            last_grid: (80, 24),
            servers_path,
            settings_path,
            gui_path,
            servers,
            settings,
            gcfg,
            tabs: Vec::new(),
            active_tab: 0,
            selected_server: 0,
            form: None,
            form_open: false,
            quick: None,
            quick_open: false,
            delete_confirm: None,
            question: None,
            settings_open: false,
            about_open: false,
            prev_active_tab: 0,
            server_sort: None,
            update_ui: UpdateUi::Closed,
            update_rx: None,
            quit_confirm: false,
            status_msg: None,
            autoconnect_done: false,
            deferred_close: None,
            deferred_new_cmd: false,
            hk_close_down: false,
            hk_new_down: false,
        }
    }

    fn lang(&self) -> Language {
        self.settings.language
    }

    fn persist_servers(&mut self) {
        if let Err(e) = crate::xconfig::save(&self.servers_path, &self.servers) {
            self.toast(format!("save failed: {e}"));
        }
    }

    fn toast(&mut self, msg: String) {
        self.status_msg = Some((msg, std::time::Instant::now()));
    }

    // -- actions -----------------------------------------------------------

    /// click handler for the sort button, 3-state: name asc → desc → natural
    /// order. "natural" = the order persisted in servers.json (manual ordering
    /// via 上移/下移); sorting is a view and is NOT written back.
    fn cycle_server_sort(&mut self) {
        let selected_name = self
            .servers
            .get(self.selected_server)
            .map(|s| s.name.clone());
        self.server_sort = match self.server_sort {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        match self.server_sort {
            Some(true) => self.servers.sort_by_key(|s| s.name.to_lowercase()),
            Some(false) => {
                self.servers.sort_by_key(|s| s.name.to_lowercase());
                self.servers.reverse();
            }
            None => self.servers = crate::xconfig::load(&self.servers_path),
        }
        if let Some(n) = selected_name {
            if let Some(pos) = self.servers.iter().position(|s| s.name == n) {
                self.selected_server = pos;
            }
        }
    }

    fn connect_server(&mut self, idx: usize) {
        let Some(server) = self.servers.get(idx).cloned() else { return };
        self.spawn_connect_tab(server);
    }

    fn spawn_connect_tab(&mut self, server: Server) {
        let mut opts = resolve_opts(&server, &self.settings);
        // Headless test mode (XXSSHG_AUTOCONNECT): TOFU-trust into the standard
        // known_hosts file so no host-key dialog blocks automated runs.
        if std::env::var("XXSSHG_AUTOCONNECT").is_ok() {
            opts.known_hosts_add = Some(
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".xxssh")
                    .join("known_hosts")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        let name = server.name.clone();
        let status = tpl(
            tr(self.lang(), "status_connecting"),
            &[("host", &server.host), ("port", &server.port.to_string())],
        );
        log::info!("connect_server: spawning");
        let (cols, rows) = self.last_grid;
        let (result_rx, request_rx) = session::spawn_connect(&self.rt, server.clone(), opts, cols, rows);
        self.tabs.push(Tab::Connecting {
            name,
            result_rx,
            request_rx,
            status,
            server,
        });
        self.active_tab = self.tabs.len() - 1;
    }

    /// Reconnect a remote tab in place (SSH terminal / SFTP / failed connect);
    /// local shells have no server → no-op
    fn reconnect_tab(&mut self, idx: usize) {
        let server = match self.tabs.get(idx) {
            Some(Tab::Open { server: Some(s), .. }) => s.clone(),
            Some(Tab::Failed { server: Some(s), .. }) => s.clone(),
            Some(Tab::Sftp { st }) => st.server.clone(),
            _ => return,
        };
        // SFTP tab: tear down the old session, respawn SftpConnecting in place
        if matches!(self.tabs.get(idx), Some(Tab::Sftp { .. })) {
            let old = std::mem::replace(
                &mut self.tabs[idx],
                Tab::Failed { name: String::new(), error: String::new(), server: None },
            );
            if let Tab::Sftp { st } = old {
                st.cleanup();
                let _ = st.close_tx.send(());
            }
            let name = format!("SFTP: {}", server.name);
            let host = server.host.clone();
            let opts = resolve_opts(&server, &self.settings);
            let (result_rx, request_rx) =
                crate::session::spawn_sftp(&self.rt, server.clone(), opts);
            self.tabs[idx] = Tab::SftpConnecting {
                name,
                result_rx,
                request_rx,
                status: tpl(tr(self.lang(), "sftp_connecting"), &[("host", &host)]),
                server,
            };
            self.active_tab = idx;
            return;
        }
        let mut opts = resolve_opts(&server, &self.settings);
        if std::env::var("XXSSHG_AUTOCONNECT").is_ok() {
            opts.known_hosts_add = Some(
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".xxssh")
                    .join("known_hosts")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        let name = server.name.clone();
        let status = tpl(
            tr(self.lang(), "status_connecting"),
            &[("host", &server.host), ("port", &server.port.to_string())],
        );
        log::info!("reconnect_tab: respawning '{}'", server.name);
        crate::diag::log(&format!("reconnect requested: '{}'", server.name));
        let (cols, rows) = self.last_grid;
        let (result_rx, request_rx) = session::spawn_connect(&self.rt, server.clone(), opts, cols, rows);
        self.tabs[idx] = Tab::Connecting {
            name,
            result_rx,
            request_rx,
            status,
            server,
        };
        self.active_tab = idx;
    }

    /// Open a local shell (CMD / PowerShell / $SHELL) in a new tab
    fn open_local_shell(&mut self, kind: ShellKind) {
        let (cols, rows) = self.last_grid;
        match local::spawn(&self.rt, kind, cols, rows) {
            Ok(handle) => {
                let input_tx = handle.input_tx.clone();
                let (term, title_rx, bell_rx) =
                    Terminal::new(cols, rows, self.gcfg.scrollback_lines, input_tx);
                let name = match kind {
                    ShellKind::Cmd => tpl(tr(self.lang(), "local_cmd"), &[]),
                    ShellKind::PowerShell => tpl(tr(self.lang(), "local_pwsh"), &[]),
                    ShellKind::DefaultShell => tpl(tr(self.lang(), "local_shell"), &[]),
                };
                self.tabs.push(Tab::Open {
                    name,
                    term,
                    title_rx,
                    bell_rx,
                    handle,
                    resize_sent: false,
                    closed: None,
                    bell_flash: false,
                    server: None,
                });
                self.active_tab = self.tabs.len() - 1;
                self.prev_active_tab = self.active_tab; // Terminal::new starts focus_pending=true
            }
            Err(e) => self.toast(e),
        }
    }

    /// Open an SFTP file-manager tab for the server
    fn open_sftp_for(&mut self, idx: usize) {
        let Some(server) = self.servers.get(idx).cloned() else { return };
        let opts = resolve_opts(&server, &self.settings);
        let name = format!("SFTP: {}", server.name);
        let host = server.host.clone();
        let (result_rx, request_rx) = crate::session::spawn_sftp(&self.rt, server.clone(), opts);
        self.tabs.push(Tab::SftpConnecting {
            name,
            result_rx,
            request_rx,
            status: tpl(tr(self.lang(), "sftp_connecting"), &[("host", &host)]),
            server,
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, idx: usize) {
        let tab = self.tabs.remove(idx);
        crate::diag::log(&format!("tab closed: {}", tab.name()));
        match tab {
            Tab::Open { handle, .. } => {
                let _ = handle.close_tx.send(());
            }
            Tab::Sftp { st, .. } => {
                st.cleanup();
                let _ = st.close_tx.send(());
            }
            _ => {}
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    fn has_open_session(&self) -> bool {
        self.tabs
            .iter()
            .any(|t| matches!(t, Tab::Open { closed: None, .. } | Tab::Sftp { .. }))
    }

    // -- self-update ---------------------------------------------------------

    fn start_update_check(&mut self) {
        self.update_ui = UpdateUi::Checking;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        std::thread::spawn(move || {
            let msg = match crate::update::check() {
                Ok(c) => UpdateMsg::Checked(Ok(c)),
                Err(e) => UpdateMsg::Checked(Err(e)),
            };
            let _ = tx.send(msg);
        });
    }

    fn start_update_download(&mut self, latest: String, url: String, sha256: String) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let progress = move |done: u64, total: u64| {
                let _ = progress_tx.send(UpdateMsg::Progress(done, total));
            };
            let res = crate::update::download_and_install(&url, &sha256, &progress);
            let _ = tx.send(UpdateMsg::Installed(res));
        });
        self.update_ui = UpdateUi::Downloading { latest, done: 0, total: 0 };
    }

    /// drain worker-thread messages into the dialog state
    fn poll_update(&mut self) {
        let Some(rx) = &self.update_rx else { return };
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UpdateMsg::Checked(Ok(crate::update::UpdateCheck::UpToDate { version, .. })) => {
                    self.update_ui = UpdateUi::UpToDate { version };
                }
                UpdateMsg::Checked(Ok(crate::update::UpdateCheck::Available { latest, url, sha256 })) => {
                    self.update_ui = UpdateUi::Available { latest, url, sha256 };
                }
                UpdateMsg::Checked(Err(e)) => {
                    self.update_ui = UpdateUi::Failed { msg: e };
                }
                UpdateMsg::Progress(done, total) => {
                    if let UpdateUi::Downloading { done: d, total: t, .. } = &mut self.update_ui {
                        *d = done;
                        *t = total;
                    }
                }
                UpdateMsg::Installed(Ok(())) => {
                    if let UpdateUi::Downloading { latest, .. } = &self.update_ui {
                        let version = latest.clone();
                        self.update_ui = UpdateUi::Done { version };
                    }
                }
                UpdateMsg::Installed(Err(e)) => {
                    self.update_ui = UpdateUi::Failed { msg: e };
                }
            }
        }
        // worker messages are finished in these states — drop the channel
        if matches!(
            self.update_ui,
            UpdateUi::UpToDate { .. } | UpdateUi::Available { .. } | UpdateUi::Done { .. } | UpdateUi::Failed { .. }
        ) {
            self.update_rx = None;
        }
    }

    /// the ☰ "Check for updates" window
    fn update_dialog(&mut self, ui: &mut egui::Ui) {
        self.poll_update();
        if matches!(self.update_ui, UpdateUi::Closed) {
            return;
        }
        let lang = self.lang();
        let current = env!("CARGO_PKG_VERSION");
        let downloading = matches!(self.update_ui, UpdateUi::Downloading { .. });
        // no X button while downloading: the update cannot be cancelled
        // (NB: open=false would hide the whole window — not what we want)
        let mut open = true;
        let mut win = egui::Window::new(tpl(tr(lang, "update_title"), &[]))
            .id(egui::Id::new("update_win"))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false);
        if !downloading {
            win = win.open(&mut open);
        }
        let mut action: Option<UpdateAction> = None;
        win.show(ui, |ui| {
                match &self.update_ui {
                    UpdateUi::Checking => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tpl(tr(lang, "update_checking"), &[]));
                        });
                    }
                    UpdateUi::UpToDate { version } => {
                        ui.label(tpl(tr(lang, "update_uptodate"), &[("version", version)]));
                    }
                    UpdateUi::Available { latest, .. } => {
                        ui.label(tpl(
                            tr(lang, "update_available"),
                            &[("latest", latest), ("current", current)],
                        ));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button(tpl(tr(lang, "update_start"), &[])).clicked() {
                                action = Some(UpdateAction::Start);
                            }
                            if ui.button(tpl(tr(lang, "btn_cancel"), &[])).clicked() {
                                action = Some(UpdateAction::Cancel);
                            }
                        });
                    }
                    UpdateUi::Downloading { done, total, .. } => {
                        ui.label(tpl(
                            tr(lang, "update_downloading"),
                            &[
                                ("done", &format!("{:.1}", *done as f32 / 1048576.0)),
                                ("total", &format!("{:.1}", *total as f32 / 1048576.0)),
                            ],
                        ));
                        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).show_percentage());
                        ui.ctx().request_repaint();
                    }
                    UpdateUi::Done { version } => {
                        ui.label(tpl(tr(lang, "update_done"), &[("version", version)]));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button(tpl(tr(lang, "update_restart"), &[])).clicked() {
                                action = Some(UpdateAction::Restart);
                            }
                            if ui.button(tpl(tr(lang, "update_later"), &[])).clicked() {
                                action = Some(UpdateAction::Cancel);
                            }
                        });
                    }
                    UpdateUi::Failed { msg } => {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            tpl(tr(lang, "update_failed"), &[("err", msg)]),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button(tpl(tr(lang, "btn_retry"), &[])).clicked() {
                                action = Some(UpdateAction::Check);
                            }
                            if ui.button(tpl(tr(lang, "btn_cancel"), &[])).clicked() {
                                action = Some(UpdateAction::Cancel);
                            }
                        });
                    }
                    UpdateUi::Closed => {}
                }
            });
        // X button close (not shown during a download)
        if !open && !downloading {
            action = Some(UpdateAction::Cancel);
        }
        let start = if let UpdateUi::Available { latest, url, sha256 } = &self.update_ui {
            Some((latest.clone(), url.clone(), sha256.clone()))
        } else {
            None
        };
        match action {
            Some(UpdateAction::Check) => self.start_update_check(),
            Some(UpdateAction::Start) => {
                if let Some((latest, url, sha256)) = start {
                    self.start_update_download(latest, url, sha256);
                }
            }
            Some(UpdateAction::Restart) => crate::update::restart(),
            Some(UpdateAction::Cancel) => {
                self.update_ui = UpdateUi::Closed;
                self.update_rx = None;
            }
            None => {}
        }
    }

    // -- per-frame polling ---------------------------------------------------

    fn poll_tabs(&mut self) {
        let lang = self.lang();
        // 1. Connect-time questions (password / passphrase / host key)
        for i in 0..self.tabs.len() {
            if let Tab::Connecting { request_rx, .. } = &mut self.tabs[i] {
                if self.question.is_none() {
                    if let Some(req) = request_rx.try_recv().ok() {
                        self.question = Some(PendingQuestion {
                            question: match req {
                                ConnectRequest::Password { user, host, respond } => {
                                    Question::Password { user, host, respond: Some(respond), input: String::new() }
                                }
                                ConnectRequest::Passphrase { path, respond } => {
                                    Question::Passphrase { path, respond: Some(respond), input: String::new() }
                                }
                                ConnectRequest::HostKey { host, port, fingerprint, changed, respond } => {
                                    Question::HostKey { host, port, fingerprint, changed, respond: Some(respond) }
                                }
                            },
                        });
                    }
                }
            }
        }

        // 1b. SFTP connecting: auth questions + results
        for i in 0..self.tabs.len() {
            if let Tab::SftpConnecting { request_rx, .. } = &mut self.tabs[i] {
                if self.question.is_none() {
                    if let Some(req) = request_rx.try_recv().ok() {
                        self.question = Some(PendingQuestion {
                            question: match req {
                                ConnectRequest::Password { user, host, respond } => {
                                    Question::Password { user, host, respond: Some(respond), input: String::new() }
                                }
                                ConnectRequest::Passphrase { path, respond } => {
                                    Question::Passphrase { path, respond: Some(respond), input: String::new() }
                                }
                                ConnectRequest::HostKey { host, port, fingerprint, changed, respond } => {
                                    Question::HostKey { host, port, fingerprint, changed, respond: Some(respond) }
                                }
                            },
                        });
                    }
                }
            }
        }

        // 2. Connect results
        let mut connect_results: Vec<(usize, String, Result<SessionHandle, ConnectError>, bool, Server)> =
            Vec::new();
        for i in 0..self.tabs.len() {
            if let Tab::Connecting { result_rx, name, server, .. } = &mut self.tabs[i] {
                match result_rx.try_recv() {
                    Ok(res) => {
                        log::info!("connect result: ok={}", res.is_ok());
                        connect_results.push((i, name.clone(), res, false, server.clone()));
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        log::error!("connect task died without a result (panic?)");
                        connect_results.push((
                            i,
                            name.clone(),
                            Err(ConnectError::Other("connect task crashed (see log)".into())),
                            true,
                            server.clone(),
                        ));
                    }
                    Err(_) => {}
                }
            }
        }
        // apply in reverse so indices stay valid
        for (i, name, res, _, server) in connect_results.into_iter().rev() {
            match res {
                Ok(handle) => {
                    let input_tx = handle.input_tx.clone();
                    let (term, title_rx, bell_rx) =
                        Terminal::new(80, 24, self.gcfg.scrollback_lines, input_tx);
                    self.tabs[i] = Tab::Open {
                        name,
                        term,
                        title_rx,
                        bell_rx,
                        handle,
                        resize_sent: false,
                        closed: None,
                        bell_flash: false,
                        server: Some(server),
                    };
                }
                Err(e) => {
                    let msg = e.message(lang);
                    self.tabs[i] = Tab::Failed { name, error: msg, server: Some(server) };
                }
            }
        }

        // 2b. SFTP connect results (bounded borrow: try_recv first, then re-own)
        for i in 0..self.tabs.len() {
            let res = if let Tab::SftpConnecting { result_rx, .. } = &mut self.tabs[i] {
                result_rx.try_recv().ok()
            } else {
                None
            };
            if let Some(res) = res {
                let (name, server) = match self.tabs.get(i) {
                    Some(Tab::SftpConnecting { name, server, .. }) => (name.clone(), server.clone()),
                    _ => continue,
                };
                match res {
                    Ok(client) => {
                        let local_root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
                        let st = SftpTab::new(
                            name,
                            client.sftp,
                            client.close_tx,
                            self.rt.clone(),
                            self.lang(),
                            local_root,
                            server,
                        );
                        self.tabs[i] = Tab::Sftp { st: Box::new(st) };
                    }
                    Err(e) => {
                        let msg = e.message(lang);
                        self.tabs[i] = Tab::Failed { name, error: msg, server: Some(server) };
                    }
                }
            }
        }

        // 3. Drain PTY output / titles / bells / close events of open tabs
        for i in 0..self.tabs.len() {
            if let Tab::Open { term, title_rx, bell_rx, handle, closed, bell_flash, .. } =
                &mut self.tabs[i]
            {
                while let Some(bytes) = handle.output_rx.try_recv().ok() {
                    {
                        let head: String = bytes.iter().take(32).map(|&b| format!("{:02x} ", b)).collect();
                        crate::term::diag_log(&format!("pty-out: {} bytes: {}", bytes.len(), head));
                    }
                    if let Ok(path) = std::env::var("XXSSHG_PTY_DUMP") {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                            let _ = f.write_all(&bytes);
                        }
                    }
                    term.feed(&bytes);
                }
                while let Some(title) = title_rx.try_recv().ok() {
                    term.title = title;
                }
                if bell_rx.try_recv().is_ok() {
                    term.bell = true;
                }
                if let Some(SessionEvent::Closed { exit_code, reason }) = handle.event_rx.try_recv().ok() {
                    log::info!("session closed: {reason} (exit {exit_code:?})");
                    *closed = Some(match exit_code {
                        Some(c) => format!("{reason} (exit {c})"),
                        None => reason,
                    });
                }
                if term.bell {
                    term.bell = false;
                    *bell_flash = true;
                }
            }
        }
    }

    // -- ui ------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        // Action buttons live in a bottom sub-panel: whatever the UI zoom level,
        // they are pinned to the bottom of the sidebar and can never be pushed
        // out of the visible area by the server list.
        egui::Panel::bottom(egui::Id::new("sidebar_actions"))
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(4.0);

                // Primary action: Connect + hamburger (more) menu
                let can_connect =
                    !self.servers.is_empty() && self.selected_server < self.servers.len();
                ui.horizontal(|ui| {
                    let lang = self.lang();
                    // Reserve fixed slots for the two square buttons (⚡ / ☰) plus
                    // inter-item spacing, so the row can never overflow the panel
                    // no matter what width is available.
                    let small_w = 26.0;
                    let spacing = ui.style().spacing.item_spacing.x;
                    let conn_w = (ui.available_width() - 2.0 * small_w - 2.0 * spacing - 8.0).max(60.0);
                    ui.add_enabled_ui(can_connect, |ui| {
                        let connect_btn = ui.add_sized(
                            [conn_w, 26.0],
                            egui::Button::new(
                                egui::RichText::new(tpl(tr(lang, "btn_connect"), &[])).strong(),
                            ),
                        );
                        if connect_btn.clicked() {
                            self.connect_server(self.selected_server);
                        }
                    });
                    let quick_btn = ui.add_sized(
                        [small_w, 26.0],
                        egui::Button::new("⚡"),
                    ).on_hover_text(tpl(tr(lang, "qc_title"), &[]));
                    if quick_btn.clicked() {
                        self.quick = Some(QuickForm::from_gcfg(&self.gcfg));
                        self.quick_open = true;
                    }
                    // 0.36: MenuButton renders its own button — size it via
                    // Button::min_size so it matches the lightning button
                    let menu_btn = egui::Button::new("☰")
                        .min_size(egui::vec2(small_w, 26.0));
                    egui::containers::menu::MenuButton::from_button(menu_btn).ui(ui, |ui| {
                        if ui.button(tpl(tr(lang, "update_title"), &[])).clicked() {
                            self.start_update_check();
                            ui.close();
                        }
                        if ui.button(tpl(tr(lang, "menu_clear"), &[])).clicked() {
                            if let Some(t) = self.tabs.get_mut(self.active_tab) {
                                if let Tab::Open { term, .. } = t {
                                    term.clear_scrollback();
                                }
                            }
                            ui.close();
                        }
                        if ui.button(tpl(tr(lang, "settings_title"), &[])).clicked() {
                            self.settings_open = true;
                            ui.close();
                        }
                        if ui.button(tpl(tr(lang, "menu_about"), &[])).clicked() {
                            self.about_open = true;
                            ui.close();
                        }
                    });
                });
                ui.add_space(4.0);

                // Row: New / Edit / Delete
                ui.horizontal(|ui| {
                    let w = [(ui.available_width() - 12.0) / 3.0, 22.0];
                    if ui
                        .add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_new"), &[])))
                        .clicked()
                    {
                        self.form = Some(ServerForm::new());
                        self.form_open = true;
                    }
                    let can_edit =
                        !self.servers.is_empty() && self.selected_server < self.servers.len();
                    ui.add_enabled_ui(can_edit, |ui| {
                        if ui
                            .add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_edit"), &[])))
                            .clicked()
                        {
                            let srv = self.servers[self.selected_server].clone();
                            self.form = Some(ServerForm::from_server(self.selected_server, &srv));
                            self.form_open = true;
                        }
                        if ui
                            .add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_delete"), &[])))
                            .clicked()
                        {
                            self.delete_confirm = Some(self.selected_server);
                        }
                    });
                });
                ui.add_space(4.0);
            });

        // Heading + local shells + server list fill the remaining space
        ui.add_space(6.0);
        ui.heading(tpl(tr(self.lang(), "list_title"), &[]));
        ui.add_space(4.0);
        ui.separator();

        // Local shells (never part of servers.json)
        ui.label(egui::RichText::new(tpl(tr(self.lang(), "local_title"), &[])).weak().small());
        ui.horizontal_wrapped(|ui| {
            let shells: Vec<ShellKind> = if cfg!(windows) {
                vec![ShellKind::Cmd, ShellKind::PowerShell]
            } else {
                vec![ShellKind::DefaultShell]
            };
            for kind in shells {
                let label = match kind {
                    ShellKind::Cmd => tpl(tr(self.lang(), "local_cmd"), &[]),
                    ShellKind::PowerShell => tpl(tr(self.lang(), "local_pwsh"), &[]),
                    ShellKind::DefaultShell => tpl(tr(self.lang(), "local_shell"), &[]),
                };
                if ui.small_button(label).clicked() {
                    self.open_local_shell(kind);
                }
            }
        });
        ui.separator();
        // Heading + quick-add button
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(tpl(tr(self.lang(), "list_title"), &[]));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("＋").on_hover_text(tpl(tr(self.lang(), "btn_new"), &[])).clicked() {
                    self.form = Some(ServerForm::new());
                    self.form_open = true;
                }
                // 3-state sort by name: asc ▲ → desc ▼ → natural order.
                // Text labels throughout: the ⇅ glyph is missing from the UI
                // font and renders as a hollow box.
                let label = match self.server_sort {
                    Some(true) => format!("{} ▲", tr(self.lang(), "sort_name")),
                    Some(false) => format!("{} ▼", tr(self.lang(), "sort_name")),
                    None => tpl(tr(self.lang(), "srv_sort_name"), &[]),
                };
                let hover = match self.server_sort {
                    Some(true) => tpl(tr(self.lang(), "srv_sort_hover_asc"), &[]),
                    Some(false) => tpl(tr(self.lang(), "srv_sort_hover_desc"), &[]),
                    None => tpl(tr(self.lang(), "srv_sort_hover_none"), &[]),
                };
                if ui
                    .small_button(egui::RichText::new(label).small())
                    .on_hover_text(hover)
                    .clicked()
                {
                    self.cycle_server_sort();
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false]) // span the full sidebar width: scrollbar hugs the edge
            .show(ui, |ui| {
            for i in 0..self.servers.len() {
                let name = self.servers[i].name.clone();
                let host = format!("{}@{}:{}", self.servers[i].username, self.servers[i].host, self.servers[i].port);
                let resp = ui.selectable_label(
                    i == self.selected_server,
                    egui::RichText::new(format!("{name}\n  {host}")),
                );
                if resp.clicked() {
                    self.selected_server = i;
                    if resp.double_clicked() {
                        self.connect_server(i);
                    }
                }
                resp.context_menu(|ui| {
                    if ui.button(tpl(tr(self.lang(), "btn_connect"), &[])).clicked() {
                        self.connect_server(i);
                        ui.close();
                    }
                    if ui.button(tpl(tr(self.lang(), "btn_edit"), &[])).clicked() {
                        let srv = self.servers[i].clone();
                        self.form = Some(ServerForm::from_server(i, &srv));
                        self.form_open = true;
                        ui.close();
                    }
                    if ui.button(tpl(tr(self.lang(), "btn_delete"), &[])).clicked() {
                        self.delete_confirm = Some(i);
                        ui.close();
                    }
                    ui.separator();
                    if ui.add_enabled(i > 0, egui::Button::new(tpl(tr(self.lang(), "srv_move_up"), &[]))).clicked() {
                        self.servers.swap(i - 1, i);
                        if self.selected_server == i {
                            self.selected_server = i - 1;
                        } else if self.selected_server == i - 1 {
                            self.selected_server = i;
                        }
                        self.persist_servers();
                        ui.close();
                    }
                    if ui.add_enabled(i + 1 < self.servers.len(), egui::Button::new(tpl(tr(self.lang(), "srv_move_down"), &[]))).clicked() {
                        self.servers.swap(i, i + 1);
                        if self.selected_server == i {
                            self.selected_server = i + 1;
                        } else if self.selected_server == i + 1 {
                            self.selected_server = i;
                        }
                        self.persist_servers();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(tpl(tr(self.lang(), "menu_sftp"), &[])).clicked() {
                        self.open_sftp_for(i);
                        ui.close();
                    }
                });
            }
        });
    }

    fn tabs_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let mut activate: Option<usize> = None;
            let mut close: Option<usize> = None;
            let mut clear: Option<usize> = None;
            let mut reconnect: Option<usize> = None;
            for i in 0..self.tabs.len() {
                let is_active = i == self.active_tab;
                let title = self.tabs[i].name().to_string();
                let resp = ui.selectable_label(is_active, title);
                if resp.clicked() {
                    activate = Some(i);
                }
                // Right-click on a tab opens its menu (no inline × button:
                // it sat right next to the label and was easy to mis-hit)
                resp.context_menu(|ui| {
                    if self.tabs[i].reconnect_server().is_some()
                        && ui.button(tpl(tr(self.lang(), "btn_reconnect"), &[])).clicked()
                    {
                        reconnect = Some(i);
                        ui.close();
                    }
                    if matches!(self.tabs[i], Tab::Open { .. })
                        && ui.button(tpl(tr(self.lang(), "menu_clear"), &[])).clicked()
                    {
                        clear = Some(i);
                        ui.close();
                    }
                    if ui.button(tpl(tr(self.lang(), "btn_close_tab"), &[])).clicked() {
                        close = Some(i);
                        ui.close();
                    }
                });
            }
            if let Some(i) = activate {
                self.active_tab = i;
            }
            if let Some(i) = close {
                self.close_tab(i);
            }
            if let Some(i) = clear {
                if let Some(Tab::Open { term, .. }) = self.tabs.get_mut(i) {
                    term.clear_scrollback();
                }
            }
            if let Some(i) = reconnect {
                self.reconnect_tab(i);
            }
        });
        ui.separator();
    }

    fn terminal_area(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(tpl(tr(self.lang(), "app_title"), &[]));
            });
            return;
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        // tab switch (or the active tab just materialized): hand keyboard focus
        // to the terminal without requiring a click
        if self.active_tab != self.prev_active_tab {
            self.prev_active_tab = self.active_tab;
            if let Some(Tab::Open { term, .. }) = self.tabs.get_mut(self.active_tab) {
                term.focus_pending = true;
            }
        }
        let i = self.active_tab;
        let mut do_reconnect = false;
        let mut do_close = false;
        match &mut self.tabs[i] {
            Tab::Connecting { status, .. } => {
                ui.centered_and_justified(|ui| {
                    ui.label(status.clone());
                    ui.spinner();
                });
            }
            Tab::Failed { error, .. } => {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(egui::Color32::LIGHT_RED, error.clone());
                });
            }
            Tab::Sftp { st } => {
                st.poll();
                st.ui(ui);
                // OS drag&drop: files dropped anywhere in the window upload into
                // the current remote dir while an SFTP tab is active
                let dropped: Vec<PathBuf> = ui.input(|i| {
                    i.raw
                        .dropped_files
                        .iter()
                        .map(|f| f.path().to_path_buf())
                        .collect()
                });
                if !dropped.is_empty() {
                    st.handle_os_dropped(dropped);
                }
            }
            Tab::SftpConnecting { status, .. } => {
                ui.centered_and_justified(|ui| {
                    ui.label(status.clone());
                    ui.spinner();
                });
            }
            Tab::Open { term, handle, resize_sent, closed, bell_flash, server, .. } => {
                if let Some(reason) = closed.clone() {
                    let can_reconnect = server.is_some();
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(48.0);
                            ui.label(tpl(tr(self.lang(), "status_closed"), &[("reason", &reason)]));
                            ui.add_space(10.0);
                            if can_reconnect
                                && ui.button(tpl(tr(self.lang(), "btn_reconnect"), &[])).clicked()
                            {
                                do_reconnect = true;
                            }
                            if ui.button(tpl(tr(self.lang(), "btn_close_tab"), &[])).clicked() {
                                do_close = true;
                            }
                        });
                    });
                } else {
                    term.hotkey_close = crate::gconfig::parse_hotkey(&self.gcfg.hotkey_close_tab);
                    term.hotkey_new = crate::gconfig::parse_hotkey(&self.gcfg.hotkey_new_cmd);
                    let resized = term.paint(ui, self.gcfg.font_size, self.gcfg.copy_on_select, self.gcfg.invert_scrolling);
                    if let Some(zoom) = term.pending_zoom.take() {
                        if zoom.is_nan() {
                            self.gcfg.font_size = 14.0;
                        } else {
                            self.gcfg.font_size = (self.gcfg.font_size + 2.0 * zoom).clamp(9.0, 28.0);
                        }
                        let _ = crate::gconfig::save(&self.gui_path, &self.gcfg);
                    }
                    if resized || !*resize_sent {
                        let (cols, rows) = term.grid_size();
                        let _ = handle.resize_tx.send((cols, rows));
                        *resize_sent = true;
                    }
                    if *bell_flash {
                        *bell_flash = false;
                        if self.gcfg.bell == BellMode::Flash {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                    }
                }
            }
        }
        if do_close {
            self.close_tab(i);
        } else if do_reconnect {
            self.reconnect_tab(i);
        }
    }

    // -- dialogs -------------------------------------------------------------

    fn dialogs(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let dlg_lang = self.lang();
        // Server form
        if self.form.is_some() {
            let mut open = self.form_open;
            let mut save = false;
            let mut cancel = false;
            let lang = dlg_lang;
            let win = egui::Window::new(if self.form.as_ref().unwrap().editing.is_some() {
                tr(lang, "form_edit_title")
            } else {
                tr(lang, "form_new_title")
            })
            .open(&mut open)
            .collapsible(false)
            .resizable(false);
            win.show(ui, |ui| {
                let form = self.form.as_mut().unwrap();
                egui::Grid::new("server_form")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(tr(lang, "f_name"));
                        ui.text_edit_singleline(&mut form.name);
                        ui.end_row();
                        ui.label(tr(lang, "f_host"));
                        ui.text_edit_singleline(&mut form.host);
                        ui.end_row();
                        ui.label(tr(lang, "f_port"));
                        ui.add(egui::TextEdit::singleline(&mut form.port).desired_width(80.0));
                        ui.end_row();
                        ui.label(tr(lang, "f_user"));
                        ui.text_edit_singleline(&mut form.username);
                        ui.end_row();
                        ui.label(tr(lang, "f_auth"));
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut form.auth,
                                AuthMethod::Password,
                                tr(lang, "auth_password"),
                            );
                            ui.selectable_value(&mut form.auth, AuthMethod::Key, tr(lang, "auth_key"));
                        });
                        ui.end_row();
                        match form.auth {
                            AuthMethod::Password => {
                                ui.label(tr(lang, "f_password"));
                                ui.add(egui::TextEdit::singleline(&mut form.password).password(true));
                                ui.end_row();
                            }
                            AuthMethod::Key => {
                                ui.label(tr(lang, "f_key_path"));
                                ui.text_edit_singleline(&mut form.key_path);
                                ui.end_row();
                                ui.label(tr(lang, "f_key_pass"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.key_passphrase).password(true),
                                );
                                ui.end_row();
                            }
                        }
                        ui.label(tr(lang, "f_proxy"));
                        ui.text_edit_singleline(&mut form.proxy);
                        ui.end_row();
                    });
                if let Some(err) = &form.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
                ui.horizontal(|ui| {
                    if ui.button(tr(lang, "btn_save")).clicked() {
                        save = true;
                    }
                    if ui.button(tr(lang, "btn_cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
            self.form_open = open;
            if save {
                let built = self.form.as_ref().unwrap().build();
                if built.name.is_empty() {
                    self.form.as_mut().unwrap().error = Some(tr(lang, "err_name_required").into());
                } else if built.host.is_empty() {
                    self.form.as_mut().unwrap().error = Some(tr(lang, "err_host_required").into());
                } else {
                    match self.form.as_ref().unwrap().editing {
                        Some(idx) => self.servers[idx] = built,
                        None => self.servers.push(built),
                    }
                    self.persist_servers();
                    self.form = None;
                    self.form_open = false;
                }
            }
            if cancel || !open {
                self.form = None;
                self.form_open = false;
            }
        }

        // Delete confirmation
        if let Some(idx) = self.delete_confirm {
            if idx < self.servers.len() {
                let name = self.servers[idx].name.clone();
                let lang = dlg_lang;
                let mut answer: Option<bool> = None;
                egui::Window::new(tr(lang, "btn_delete"))
                    .collapsible(false)
                    .resizable(false)
                    .show(ui, |ui| {
                        ui.label(tpl(tr(lang, "confirm_delete"), &[("name", &name)]));
                        ui.horizontal(|ui| {
                            if ui.button(tr(lang, "yes")).clicked() {
                                answer = Some(true);
                            }
                            if ui.button(tr(lang, "no")).clicked() {
                                answer = Some(false);
                            }
                        });
                    });
                match answer {
                    Some(true) => {
                        self.servers.remove(idx);
                        self.selected_server =
                            self.selected_server.min(self.servers.len().saturating_sub(1));
                        self.persist_servers();
                        self.delete_confirm = None;
                    }
                    Some(false) => self.delete_confirm = None,
                    None => {}
                }
            } else {
                self.delete_confirm = None;
            }
        }

        // Connect-time question (password / passphrase / host key)
        if let Some(pq) = &mut self.question {
            let mut done = false;
            let lang = dlg_lang;
            match &mut pq.question {
                Question::Password { user, host, respond, input } => {
                    let mut submit = false;
                    let mut cancel = false;
                    egui::Window::new(tpl(tr(lang, "pwd_title"), &[("name", "")]))
                        .collapsible(false)
                        .resizable(false)
                        .show(ui, |ui| {
                            ui.label(tpl(tr(lang, "pwd_prompt"), &[("user", user), ("host", host)]));
                            let resp = ui
                                .add(egui::TextEdit::singleline(input).password(true).desired_width(260.0));
                            submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            ui.horizontal(|ui| {
                                if ui.button(tr(lang, "btn_ok")).clicked() {
                                    submit = true;
                                }
                                if ui.button(tr(lang, "btn_cancel")).clicked() {
                                    cancel = true;
                                }
                            });
                        });
                    if cancel {
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(None);
                        }
                        done = true;
                    } else if submit {
                        let v = Some(std::mem::take(input));
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(v);
                        }
                        done = true;
                    }
                }
                Question::Passphrase { path, respond, input } => {
                    let mut submit = false;
                    let mut cancel = false;
                    egui::Window::new("Passphrase")
                        .collapsible(false)
                        .resizable(false)
                        .show(ui, |ui| {
                            ui.label(format!("{path}:"));
                            let resp = ui
                                .add(egui::TextEdit::singleline(input).password(true).desired_width(260.0));
                            submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            ui.horizontal(|ui| {
                                if ui.button(tr(lang, "btn_ok")).clicked() {
                                    submit = true;
                                }
                                if ui.button(tr(lang, "btn_cancel")).clicked() {
                                    cancel = true;
                                }
                            });
                        });
                    if cancel {
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(None);
                        }
                        done = true;
                    } else if submit {
                        let v = Some(std::mem::take(input));
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(v);
                        }
                        done = true;
                    }
                }
                Question::HostKey { host, port, fingerprint, changed, respond } => {
                    let mut answer: Option<bool> = None;
                    egui::Window::new(tr(lang, "hk_title"))
                        .collapsible(false)
                        .resizable(false)
                        .show(ui, |ui| {
                            if *changed {
                                ui.colored_label(
                                    egui::Color32::LIGHT_RED,
                                    tr(lang, "hk_changed_warning"),
                                );
                            }
                            ui.label(tpl(tr(lang, "hk_unknown"), &[
                                ("host", host),
                                ("port", &port.to_string()),
                                ("fp", fingerprint),
                            ]));
                            ui.horizontal(|ui| {
                                if ui.button(tr(lang, "hk_accept")).clicked() {
                                    answer = Some(true);
                                }
                                if ui.button(tr(lang, "hk_reject")).clicked() {
                                    answer = Some(false);
                                }
                            });
                        });
                    if let Some(a) = answer {
                        if let Some(tx) = respond.take() {
                            let _ = tx.send(a);
                        }
                        done = true;
                    }
                }
            }
            if done {
                self.question = None;
            }
        }

        // Settings dialog
        if self.settings_open {
            let mut close = false;
            let lang = dlg_lang;
            egui::Window::new(tr(lang, "settings_title"))
                .collapsible(false)
                .resizable(false)
                .show(ui, |ui| {
                    egui::Grid::new("settings_grid")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(tr(lang, "s_language"));
                            egui::ComboBox::from_id_salt("lang_cb")
                                .selected_text(self.settings.language.label())
                                .show_ui(ui, |ui| {
                                    for l in Language::all() {
                                        ui.selectable_value(&mut self.settings.language, l, l.label());
                                    }
                                });
                            ui.end_row();
                            ui.label(tr(lang, "s_theme"));
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.gcfg.theme, Theme::Dark, tr(lang, "theme_dark"));
                                ui.selectable_value(&mut self.gcfg.theme, Theme::Light, tr(lang, "theme_light"));
                            });
                            ui.end_row();
                            ui.label(tr(lang, "s_font_size"));
                            ui.add(egui::Slider::new(&mut self.gcfg.font_size, 9.0..=28.0));
                            ui.end_row();
                            ui.label(tr(lang, "s_scrollback"));
                            ui.add(
                                egui::Slider::new(&mut self.gcfg.scrollback_lines, 100..=100000)
                                    .logarithmic(true),
                            );
                            ui.end_row();
                            ui.label(tr(lang, "s_copy_on_select"));
                            ui.checkbox(&mut self.gcfg.copy_on_select, "");
                            ui.end_row();
                            ui.label(tr(lang, "s_confirm_quit"));
                            ui.checkbox(&mut self.gcfg.confirm_on_quit, "");
                            ui.end_row();
                            ui.label(tr(lang, "s_bell"));
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.gcfg.bell, BellMode::Mute, tr(lang, "bell_mute"));
                                ui.selectable_value(&mut self.gcfg.bell, BellMode::Flash, tr(lang, "bell_flash"));
                                ui.selectable_value(&mut self.gcfg.bell, BellMode::Sound, tr(lang, "bell_sound"));
                            });
                            ui.end_row();
                            ui.label(tr(lang, "s_invert_scroll"));
                            ui.checkbox(&mut self.gcfg.invert_scrolling, "");
                            ui.end_row();
                            ui.label(tr(lang, "hk_close_tab"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.gcfg.hotkey_close_tab)
                                    .hint_text("Ctrl+W"),
                            );
                            ui.end_row();
                            ui.label(tr(lang, "hk_new_cmd"));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.gcfg.hotkey_new_cmd)
                                    .hint_text("Ctrl+N"),
                            );
                            ui.end_row();
                            ui.label(tr(lang, "s_font"));
                            egui::ComboBox::from_id_salt("font_cb")
                                .selected_text(if self.gcfg.font_family.is_empty() {
                                    egui::RichText::new(tpl(tr(lang, "s_font_default"), &[])).weak()
                                } else {
                                    egui::RichText::new(self.gcfg.font_family.clone())
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.gcfg.font_family,
                                        String::new(),
                                        tpl(tr(lang, "s_font_default"), &[]),
                                    );
                                    for label in self.mono_fonts.clone() {
                                        ui.selectable_value(
                                            &mut self.gcfg.font_family,
                                            label.to_string(),
                                            label,
                                        );
                                    }
                                });
                            ui.end_row();
                            ui.label(tr(lang, "s_sharp_font"));
                            ui.checkbox(&mut self.gcfg.sharp_font, "");
                            ui.end_row();
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(tr(lang, "btn_save")).clicked() {
                            let checks = vec![
                                (tr(lang, "hk_close_tab"), self.gcfg.hotkey_close_tab.clone()),
                                (tr(lang, "hk_new_cmd"), self.gcfg.hotkey_new_cmd.clone()),
                            ];
                            for (label, val) in &checks {
                                if !val.trim().is_empty()
                                    && crate::gconfig::parse_hotkey(val).is_none()
                                {
                                    self.toast(format!("{label}: {val} ?"));
                                }
                            }
                            if let Err(e) = crate::gconfig::save(&self.gui_path, &self.gcfg) {
                                self.toast(format!("save failed: {e}"));
                            }
                            if let Err(e) = save_settings(&self.settings_path, &self.settings) {
                                self.toast(format!("save failed: {e}"));
                            }
                            fonts::apply_fonts(&ctx, &self.gcfg.font_family, self.gcfg.sharp_font);
                            self.toast(tr(lang, "s_saved").into());
                            close = true;
                        }
                        if ui.button(tr(lang, "btn_cancel")).clicked() {
                            self.gcfg = crate::gconfig::load(&self.gui_path);
                            self.settings = load_settings(&self.settings_path);
                            close = true;
                        }
                    });
                });
            if close {
                self.settings_open = false;
            }
        }

        // Quick Connect dialog
        if self.quick.is_some() {
            let mut open = self.quick_open;
            let connect = false;
            let mut cancel = false;
            let lang = dlg_lang;
            let win = egui::Window::new(tr(lang, "qc_title"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false);
            win.show(ui, |ui| {
                let qc = self.quick.as_mut().unwrap();
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let mut submit_on_enter = false;
                egui::Grid::new("quick_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(tr(lang, "f_host"));
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut qc.host)
                                .hint_text("example.com")
                                .desired_width(220.0),
                        );
                        if qc.focus_host {
                            r.request_focus();
                            qc.focus_host = false;
                        }
                        submit_on_enter |= r.lost_focus() && enter;
                        ui.end_row();
                        ui.label(tr(lang, "f_port"));
                        let r = ui.add(egui::TextEdit::singleline(&mut qc.port).desired_width(80.0));
                        submit_on_enter |= r.lost_focus() && enter;
                        ui.end_row();
                        ui.label(tr(lang, "f_user"));
                        let r = ui.text_edit_singleline(&mut qc.username);
                        submit_on_enter |= r.lost_focus() && enter;
                        ui.end_row();
                        ui.label(tr(lang, "f_password"));
                        let r = ui.add(egui::TextEdit::singleline(&mut qc.password).password(true));
                        submit_on_enter |= r.lost_focus() && enter;
                        ui.end_row();
                    });
                let mut connect = connect || submit_on_enter;
                if let Some(err) = &qc.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(tr(lang, "qc_connect")).clicked() {
                        connect = true;
                    }
                    if ui.button(tr(lang, "btn_cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
            self.quick_open = open;
            if connect {
                let built = self.quick.as_ref().unwrap().build();
                if built.host.is_empty() {
                    self.quick.as_mut().unwrap().error = Some(tr(lang, "err_host_required").into());
                } else {
                    // remember host/port/user for next time (never the password)
                    self.gcfg.quick_host = built.host.clone();
                    self.gcfg.quick_port = built.port;
                    self.gcfg.quick_user = built.username.clone();
                    let _ = crate::gconfig::save(&self.gui_path, &self.gcfg);
                    let server = built;
                    self.quick = None;
                    self.quick_open = false;
                    self.spawn_connect_tab(server);
                }
            }
            if cancel || !open {
                self.quick = None;
                self.quick_open = false;
            }
        }

        // About dialog
        if self.about_open {
            let lang = self.lang();
            let mut close = false;
            egui::Window::new(tpl(tr(lang, "menu_about"), &[]))
                .collapsible(false)
                .resizable(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Image::new(egui::include_image!("../assets/xxssh-icon.png")).max_size([32.0, 32.0].into()));
                        ui.vertical(|ui| {
                            ui.heading(egui::RichText::new(format!(
                                "xxsshg v{} ({})",
                                env!("CARGO_PKG_VERSION"),
                                env!("XXSSHG_BUILD_HASH")
                            )).strong());
                            ui.label(egui::RichText::new(tr(lang, "about_text")).weak());
                        });
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(tr(lang, "btn_ok")).clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.about_open = false;
            }
        }

        // Quit confirmation
        if self.quit_confirm {
            let lang = dlg_lang;
            let mut answer: Option<bool> = None;
            egui::Window::new(tr(lang, "quit_title"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .show(ui, |ui| {
                    ui.label(tr(lang, "quit_msg"));
                    ui.horizontal(|ui| {
                        if ui.button(tr(lang, "btn_quit")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(tr(lang, "btn_cancel")).clicked() {
                            answer = Some(false);
                        }
                    });
                });
            match answer {
                Some(true) => {
                    self.capture_window_state(&ctx);
                    self.persist_gui();
                    std::process::exit(0);
                }
                Some(false) => self.quit_confirm = false,
                None => {}
            }
        }

        // Transient status message
        if let Some((msg, t)) = &self.status_msg {
            if t.elapsed().as_secs() < 3 {
                egui::Area::new(egui::Id::new("toast"))
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-10.0, -10.0])
                    .show(ui, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(egui::RichText::new(msg).color(egui::Color32::LIGHT_GREEN));
                        });
                    });
            } else {
                self.status_msg = None;
            }
        }
    }

    pub fn persist_gui(&self) {
        let _ = crate::gconfig::save(&self.gui_path, &self.gcfg);
    }

    /// Record current window geometry into gcfg (called on exit paths)
    fn capture_window_state(&mut self, ctx: &egui::Context) {
        let vi = ctx.input(|i| i.viewport().clone());
        if let Some(rect) = vi.outer_rect {
            self.gcfg.window.width = rect.width().round().max(1.0) as u32;
            self.gcfg.window.height = rect.height().round().max(1.0) as u32;
            self.gcfg.window.x = rect.min.x.round() as i32;
            self.gcfg.window.y = rect.min.y.round() as i32;
            self.gcfg.window.saved = true;
        }
        self.gcfg.window.maximized = vi.maximized.unwrap_or(false);
    }

}

impl eframe::App for XxsshgApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // App-wide hotkeys, edge-detected on the KEY STATE (not on events):
        // egui replays the same events across rendering passes and the OS
        // auto-repeats held keys — both would spam. keys_down is stateful, so
        // a rising edge (wasn't down -> down) fires exactly once per press.
        let hk_close = crate::gconfig::parse_hotkey(&self.gcfg.hotkey_close_tab);
        let hk_new = crate::gconfig::parse_hotkey(&self.gcfg.hotkey_new_cmd);
        if hk_close.is_some() || hk_new.is_some() {
            let (mods, keys_down) = ui.ctx().input(|i| (i.modifiers, i.keys_down.clone()));
            let hit = |hk: Option<(bool, bool, bool, egui::Key)>| match hk {
                Some((c, sh, al, k)) => {
                    mods.ctrl == c && mods.shift == sh && mods.alt == al && keys_down.contains(&k)
                }
                None => false,
            };
            let close_down = hit(hk_close);
            let new_down = hit(hk_new);
            crate::term::diag_log(&format!(
                "[hotkey] mods(c={} s={} a={}) keys_down={:?} | close_down={} (was {}) | new_down={} (was {}) | tabs={}",
                mods.ctrl, mods.shift, mods.alt, keys_down,
                close_down, self.hk_close_down, new_down, self.hk_new_down, self.tabs.len()
            ));
            if close_down && !self.hk_close_down {
                crate::term::diag_log("[hotkey] -> close tab (rising edge)");
                self.deferred_close =
                    Some(self.active_tab.min(self.tabs.len().saturating_sub(1)));
            }
            if new_down && !self.hk_new_down {
                crate::term::diag_log("[hotkey] -> new cmd tab (rising edge)");
                self.deferred_new_cmd = true;
            }
            self.hk_close_down = close_down;
            self.hk_new_down = new_down;
        }

        // Hotkey-initiated tab actions (deferred by the terminal widget)
        if let Some(i) = self.deferred_close.take() {
            let name = self.tabs.get(i).map(|t| t.name().to_string()).unwrap_or_default();
            self.close_tab(i);
            self.toast(format!("{name} ✕"));
        }
        if self.deferred_new_cmd {
            self.deferred_new_cmd = false;
            self.open_local_shell(ShellKind::Cmd);
        }

        // Headless test hook: XXSSHG_AUTOCONNECT=<server name> connects on startup
        if !self.autoconnect_done {
            self.autoconnect_done = true;
            if let Ok(name) = std::env::var("XXSSHG_AUTOCONNECT") {
                if let Some(idx) = self.servers.iter().position(|s| s.name == name) {
                    log::info!("autoconnect: {name}");
                    self.connect_server(idx);
                }
            }
        }

        // Poll background session activity every frame; keep repainting while
        // any tab is live so output keeps flowing even without user input.
        self.poll_tabs();
        let live = self.tabs.iter().any(|t| {
            matches!(t, Tab::Connecting { .. } | Tab::Open { closed: None, .. })
        });
        if live {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        // Theme
        match self.gcfg.theme {
            Theme::Dark => ctx.set_visuals(egui::Visuals::dark()),
            Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        }

        // Intercept the window close: confirm when sessions are open
        let closing = ctx.input(|i| i.viewport().close_requested());
        if closing {
            if !self.gcfg.confirm_on_quit || !self.has_open_session() {
                self.capture_window_state(&ctx);
                self.persist_gui();
                // No confirmation needed: let the window close (do not cancel)
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quit_confirm = true;
            }
        }

        egui::Panel::left(egui::Id::new("servers"))
            .exact_size(240.0)
            .resizable(false)
            .show(ui, |ui| {
                self.sidebar(ui);
            });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.tabs_bar(ui);
            // Track the terminal-area grid size every frame (before any tab exists)
            // so new sessions open with the correct PTY size immediately.
            let cell_w = 8.0f32.max(ui.ctx().fonts_mut(|f| {
                f.glyph_width(&egui::FontId::monospace(self.gcfg.font_size), 'M')
            }));
            let avail = ui.available_size();
            self.last_grid = (
                (((avail.x - 4.0) / cell_w).floor() as u16).max(2),
                (((avail.y - 30.0) / (self.gcfg.font_size * 1.25)).floor() as u16).max(2),
            );
            self.terminal_area(ui);
        });

        self.dialogs(ui);
        self.update_dialog(ui);
    }
}
