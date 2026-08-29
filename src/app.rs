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
    },
    Failed {
        name: String,
        error: String,
    },
}

impl Tab {
    fn name(&self) -> &str {
        match self {
            Tab::Connecting { name, .. }
            | Tab::Open { name, .. }
            | Tab::Failed { name, .. } => name,
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
    delete_confirm: Option<usize>,
    question: Option<PendingQuestion>,
    settings_open: bool,
    about_open: bool,
    quit_confirm: bool,
    status_msg: Option<(String, std::time::Instant)>,
    autoconnect_done: bool,
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
            delete_confirm: None,
            question: None,
            settings_open: false,
            about_open: false,
            quit_confirm: false,
            status_msg: None,
            autoconnect_done: false,
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

    fn connect_server(&mut self, idx: usize) {
        let Some(server) = self.servers.get(idx).cloned() else { return };
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
        log::info!("connect_server: spawning");
        let (cols, rows) = self.last_grid;
        let (result_rx, request_rx) = session::spawn_connect(&self.rt, server, opts, self.lang(), cols, rows);
        self.tabs.push(Tab::Connecting {
            name,
            result_rx,
            request_rx,
            status: tpl(
                tr(self.lang(), "status_connecting"),
                &[("host", &self.servers[idx].host), ("port", &self.servers[idx].port.to_string())],
            ),
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, idx: usize) {
        let tab = self.tabs.remove(idx);
        if let Tab::Open { handle, .. } = tab {
            let _ = handle.close_tx.send(());
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    fn has_open_session(&self) -> bool {
        self.tabs.iter().any(|t| matches!(t, Tab::Open { closed: None, .. }))
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

        // 2. Connect results
        let mut connect_results: Vec<(usize, String, Result<SessionHandle, ConnectError>, bool)> =
            Vec::new();
        for i in 0..self.tabs.len() {
            if let Tab::Connecting { result_rx, name, .. } = &mut self.tabs[i] {
                match result_rx.try_recv() {
                    Ok(res) => {
                        log::info!("connect result: ok={}", res.is_ok());
                        connect_results.push((i, name.clone(), res, false));
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        log::error!("connect task died without a result (panic?)");
                        connect_results.push((
                            i,
                            name.clone(),
                            Err(ConnectError::Other("connect task crashed (see log)".into())),
                            true,
                        ));
                    }
                    Err(_) => {}
                }
            }
        }
        // apply in reverse so indices stay valid
        for (i, name, res, _) in connect_results.into_iter().rev() {
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
                    };
                }
                Err(e) => {
                    let msg = e.message(lang);
                    self.tabs[i] = Tab::Failed { name, error: msg };
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
        ui.add_space(6.0);
        ui.heading(tpl(tr(self.lang(), "list_title"), &[]));
        ui.add_space(4.0);
        ui.separator();

        // Server list fills the remaining space.
        // NOTE: the list height is computed BEFORE entering the ScrollArea — using
        // available_height inside the closure would feed content size back into the
        // layout and grow every repaint.
        let btn_h = 78.0; // reserved height for the button area below
        let list_height = (ui.available_height() - btn_h).max(60.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_min_height(list_height);
            for i in 0..self.servers.len() {
                let name = self.servers[i].name.clone();
                let host = format!(
                    "{}@{}:{}",
                    self.servers[i].username, self.servers[i].host, self.servers[i].port
                );
                let resp = ui.selectable_label(i == self.selected_server, format!("{name}
  {host}"));
                if resp.clicked() {
                    self.selected_server = i;
                    if resp.double_clicked() {
                        self.connect_server(i);
                    }
                }
            }
        });

        ui.separator();
        ui.add_space(4.0);

        // Primary action: Connect + hamburger (more) menu
        let can_connect = !self.servers.is_empty() && self.selected_server < self.servers.len();
        ui.horizontal(|ui| {
            let lang = self.lang();
            ui.add_enabled_ui(can_connect, |ui| {
                let btn_w = ui.available_width() - 34.0;
                let connect_btn = ui.add_sized(
                    [btn_w, 26.0],
                    egui::Button::new(egui::RichText::new(tpl(tr(lang, "btn_connect"), &[])).strong()),
                );
                if connect_btn.clicked() {
                    self.connect_server(self.selected_server);
                }
            });
            ui.menu_button("☰", |ui| {
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
            if ui.add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_new"), &[]))).clicked() {
                self.form = Some(ServerForm::new());
                self.form_open = true;
            }
            let can_edit = !self.servers.is_empty() && self.selected_server < self.servers.len();
            ui.add_enabled_ui(can_edit, |ui| {
                if ui.add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_edit"), &[]))).clicked() {
                    let srv = self.servers[self.selected_server].clone();
                    self.form = Some(ServerForm::from_server(self.selected_server, &srv));
                    self.form_open = true;
                }
                if ui.add_sized(w, egui::Button::new(tpl(tr(self.lang(), "btn_delete"), &[]))).clicked() {
                    self.delete_confirm = Some(self.selected_server);
                }
            });
        });
        ui.add_space(4.0);

    }

    fn tabs_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let mut activate: Option<usize> = None;
            let mut close: Option<usize> = None;
            for i in 0..self.tabs.len() {
                let is_active = i == self.active_tab;
                let title = self.tabs[i].name().to_string();
                let resp = ui.selectable_label(is_active, title);
                if resp.clicked() {
                    activate = Some(i);
                }
                // Right-click on a tab opens its close menu (no inline × button:
                // it sat right next to the label and was easy to mis-hit)
                resp.context_menu(|ui| {
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
        let i = self.active_tab;
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
            Tab::Open { term, handle, resize_sent, closed, bell_flash, .. } => {
                if let Some(reason) = closed.clone() {
                    ui.centered_and_justified(|ui| {
                        ui.label(tpl(tr(self.lang(), "status_closed"), &[("reason", &reason)]));
                    });
                    return;
                }
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
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(tr(lang, "btn_save")).clicked() {
                            if let Err(e) = crate::gconfig::save(&self.gui_path, &self.gcfg) {
                                self.toast(format!("save failed: {e}"));
                            }
                            if let Err(e) = save_settings(&self.settings_path, &self.settings) {
                                self.toast(format!("save failed: {e}"));
                            }
                            fonts::apply_fonts(&ctx, &self.gcfg.font_family);
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
    }
}
