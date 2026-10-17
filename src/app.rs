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
use crate::tunnel::{self, ForwardSpec};
use crate::term::Terminal;
use crate::termup::{TermUp, UpState};
use crate::xconfig::{load_settings, save_settings, AppSettings, AuthMethod, Server};

/// Local session hostname as the OSC 9;9 hooks report it (%COMPUTERNAME%);
/// empty on non-Windows: local-shell hooks are Windows-only anyway
fn local_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

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
        /// drag&drop upload: lazy SFTP channel + target resolution + transfers
        up: TermUp,
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
    /// Tunnels-only connect in progress (no terminal will open)
    TunnelConnecting {
        name: String,
        result_rx: oneshot::Receiver<Result<session::TunnelHandle, ConnectError>>,
        request_rx: mpsc::UnboundedReceiver<ConnectRequest>,
        status: String,
        server: Server,
    },
    /// Live tunnels-only session: forwards up, no terminal
    Tunnels {
        name: String,
        controller: tunnel::SharedController,
        event_rx: mpsc::UnboundedReceiver<SessionEvent>,
        closed: Option<String>,
        close_tx: oneshot::Sender<()>,
        server: Server,
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
            Tab::TunnelConnecting { name, .. } | Tab::Tunnels { name, .. } => name,
        }
    }

    /// server this tab belongs to, when a reconnect makes sense
    /// (local shells have none; already-connecting tabs don't need one)
    fn reconnect_server(&self) -> Option<&Server> {
        match self {
            Tab::Open { server: Some(s), .. } => Some(s),
            Tab::Failed { server: Some(s), .. } => Some(s),
            Tab::Sftp { st } => Some(&st.server),
            Tab::Tunnels { server, .. } => Some(server),
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
    /// Port forwards, one D:/L:/R: entry per line
    tunnels: String,
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
            tunnels: String::new(),
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
            tunnels: s.tunnels.join("
"),
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
            tunnels: self
                .tunnels
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
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
            tunnels: Vec::new(),
        }
    }
}

/// Quick-tunnels dialog state: ad-hoc forwards against a saved server or a
/// temporary user@host, "this time only" or merged into the server's profile.
struct QuickTunnelForm {
    /// index into servers; == servers.len() means the temporary entry
    sel: usize,
    temp_host: String,
    temp_port: String,
    temp_user: String,
    temp_password: String,
    text: String,
    error: Option<String>,
}

impl QuickTunnelForm {
    fn new(servers_len: usize, sel: usize, prefill: Option<String>) -> Self {
        Self {
            sel: sel.min(servers_len),
            temp_host: String::new(),
            temp_port: "22".into(),
            temp_user: "root".into(),
            temp_password: String::new(),
            text: prefill.unwrap_or_default(),
            error: None,
        }
    }

    fn build_temp_server(&self) -> Server {
        let host = self.temp_host.trim().to_string();
        Server {
            name: format!("{host}:{}", self.temp_port.trim()),
            host,
            port: self.temp_port.trim().parse().unwrap_or(22),
            username: self.temp_user.trim().to_string(),
            auth: AuthMethod::Password,
            password: self.temp_password.clone(),
            key_path: String::new(),
            key_passphrase: String::new(),
            proxy: String::new(),
            tunnels: Vec::new(),
        }
    }
}

/// One inline add-forward row (tunnels tab + popup share the layout)
struct AddRowForm {
    kind: char,
    bind: String,
    port: String,
    host: String,
    rport: String,
    error: Option<String>,
}

impl AddRowForm {
    fn new() -> Self {
        Self {
            kind: 'D',
            bind: String::new(),
            port: String::new(),
            host: String::new(),
            rport: String::new(),
            error: None,
        }
    }

    fn build(&self) -> Result<ForwardSpec, String> {
        tunnel::spec_from_parts(self.kind, &self.bind, &self.port, &self.host, &self.rport)
    }
}

/// Popup bound to one live session's forwards (terminal tab "Tunnels...")
struct TunnelPopup {
    title: String,
    controller: tunnel::SharedController,
    /// server for "save to profile" (None = temporary session)
    server: Option<Server>,
    add: Option<AddRowForm>,
    actions: Vec<TunnelAction>,
}

/// UI intent collected by [`tunnel_panel_ui`], executed after the borrow ends.
enum TunnelAction {
    Add(tunnel::SharedController, ForwardSpec),
    Remove(tunnel::SharedController, u64),
    SaveServer(ForwardSpec),
}

/// True when the quick-tunnel temporary server has no usable host name.
fn text_host_empty(temp: &Option<Server>) -> bool {
    temp.as_ref().map(|srv| srv.host.trim().is_empty()).unwrap_or(true)
}

fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    let b = n as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{n} B")
    }
}

/// Shared renderer for live forwards of one session: the tunnels-only tab and
/// the terminal-tab "Tunnels..." popup both use it. Collects intents into
/// `actions` (the caller applies them once `self` is borrowable again).
fn tunnel_panel_ui(
    ui: &mut egui::Ui,
    lang: Language,
    id_prefix: &str,
    ctrl: &tunnel::SharedController,
    add: &mut Option<AddRowForm>,
    save_server: Option<&Server>,
    actions: &mut Vec<TunnelAction>,
) {
    let snaps = ctrl.list();
    if snaps.is_empty() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tr(lang, "tnl_empty")).weak());
        ui.add_space(8.0);
    }
    let rows_id = egui::Id::new(format!("{id_prefix}_tunnel_rows"));
    let add_id = egui::Id::new(format!("{id_prefix}_tunnel_add_grid"));
    egui::Grid::new(rows_id)
        .num_columns(4)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for sp in snaps {
                ui.label(egui::RichText::new("\u{25cf}").color(egui::Color32::LIGHT_GREEN));
                ui.label(egui::RichText::new(&sp.desc).monospace());
                let stats = if sp.conns > 0 {
                    format!(
                        "{}  \u{2191}{} \u{2193}{}",
                        tpl(tr(lang, "tnl_conns_n"), &[("n", &sp.conns.to_string())]),
                        fmt_bytes(sp.up),
                        fmt_bytes(sp.down)
                    )
                } else {
                    tpl(tr(lang, "tnl_conns_n"), &[("n", "0")])
                };
                ui.label(egui::RichText::new(stats).weak().small());
                ui.horizontal(|ui| {
                    if save_server.is_some() {
                        let spec = sp.spec.clone();
                        if ui
                            .small_button("\u{1f4be}")
                            .on_hover_text(tr(lang, "qt_save"))
                            .clicked()
                        {
                            actions.push(TunnelAction::SaveServer(spec));
                        }
                    }
                    let id = sp.id;
                    let c = ctrl.clone();
                    if ui
                        .small_button("\u{2715}")
                        .on_hover_text(tr(lang, "tnl_remove_hint"))
                        .clicked()
                    {
                        actions.push(TunnelAction::Remove(c, id));
                    }
                });
                ui.end_row();
            }
        });
    ui.add_space(6.0);
    if add.is_none() && ui.button(tpl(tr(lang, "tnl_add"), &[])).clicked() {
        *add = Some(AddRowForm::new());
    }
    if let Some(f) = add {
        ui.separator();
        egui::Grid::new(add_id)
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label(tr(lang, "tnl_kind"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut f.kind, 'D', "D SOCKS5");
                    ui.selectable_value(&mut f.kind, 'L', tr(lang, "tnl_kind_l"));
                    ui.selectable_value(&mut f.kind, 'R', tr(lang, "tnl_kind_r"));
                });
                ui.end_row();
                ui.label(tr(lang, "tnl_bind"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut f.bind)
                            .hint_text("127.0.0.1")
                            .desired_width(110.0),
                    );
                    ui.label(egui::RichText::new(tr(lang, "tnl_bind_hint")).weak().small());
                });
                ui.end_row();
                ui.label(tr(lang, "col_port"));
                ui.add(egui::TextEdit::singleline(&mut f.port).desired_width(80.0));
                ui.end_row();
                if f.kind.eq_ignore_ascii_case(&'D') {
                    ui.label(tr(lang, "tnl_target"));
                    ui.label(egui::RichText::new(tr(lang, "tnl_target_d")).weak());
                    ui.end_row();
                } else {
                    let label = if f.kind.eq_ignore_ascii_case(&'R') {
                        tr(lang, "tnl_target_r")
                    } else {
                        tr(lang, "tnl_target_l")
                    };
                    ui.label(label);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.host).desired_width(140.0));
                        ui.label(":");
                        ui.add(egui::TextEdit::singleline(&mut f.rport).desired_width(60.0));
                    });
                    ui.end_row();
                }
            });
        if let Some(err) = &f.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err.clone());
        }
        let mut done = false;
        ui.horizontal(|ui| {
            if ui.button(tr(lang, "tnl_do_add")).clicked() {
                match f.build() {
                    Ok(spec) => {
                        actions.push(TunnelAction::Add(ctrl.clone(), spec));
                        done = true;
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            if ui.button(tr(lang, "btn_cancel")).clicked() {
                done = true;
            }
        });
        if done {
            *add = None;
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
    /// Last time any tab received PTY output — drives the adaptive repaint
    /// heartbeat (fast while output flows, slow when idle)
    last_output: Option<std::time::Instant>,
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
    /// Quick-tunnels dialog (ad-hoc forwards, this-time-only or saved to server)
    quick_tnl: Option<QuickTunnelForm>,
    quick_tnl_open: bool,
    /// Tunnel manager popup bound to one live session (terminal tab "Tunnels…")
    tunnel_popup: Option<TunnelPopup>,
    /// The one inline add-forward form of the tunnels tab (None = hidden)
    tunnel_add_form: Option<AddRowForm>,
    /// Async add/remove feedback from tunnel ops -> toasts
    ctrl_fb_tx: mpsc::UnboundedSender<String>,
    ctrl_fb_rx: mpsc::UnboundedReceiver<String>,
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
        let (ctrl_fb_tx, ctrl_fb_rx) = mpsc::unbounded_channel();
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
            last_output: None,
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
            quick_tnl: None,
            quick_tnl_open: false,
            tunnel_popup: None,
            tunnel_add_form: None,
            ctrl_fb_tx,
            ctrl_fb_rx,
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

    /// Tunnels-only connect for a server row (context menu). Refused with a
    /// toast when the profile has no `tunnels` entries or one is malformed.
    fn connect_server_tunnels(&mut self, idx: usize) {
        let Some(server) = self.servers.get(idx).cloned() else { return };
        match tunnel::parse_tunnel_list(&server.tunnels) {
            Ok(f) if !f.is_empty() => self.spawn_tunnel_tab(server, Some(f)),
            Ok(_) => self.toast(tr(self.lang(), "tnl_none").to_string()),
            Err(e) => self.toast(tpl(tr(self.lang(), "err_tunnel"), &[("e", &e)])),
        }
    }

    /// Parse a profile's `tunnels` for a terminal connect; on a bad entry shows
    /// a Failed tab (the connect itself never starts half-configured).
    fn forwards_for(&self, server: &Server) -> Result<Vec<ForwardSpec>, String> {
        tunnel::parse_tunnel_list(&server.tunnels)
    }
    /// Open the tunnel manager popup for one live session (terminal tab menu)
    fn open_tunnel_popup(&mut self, idx: usize) {
        let Some(Tab::Open { handle, server, name, .. }) = self.tabs.get(idx) else {
            return;
        };
        let Some(ctrl) = handle.tunnel.clone() else {
            return;
        };
        let title = tpl(tr(self.lang(), "tnl_title"), &[("name", name.as_str())]);
        self.tunnel_popup = Some(TunnelPopup {
            title,
            controller: ctrl,
            server: server.clone(),
            add: None,
            actions: Vec::new(),
        });
    }

    /// Apply tunnel panel intents: async add/remove spawn on the runtime and
    /// toast their result; "save" merges the rule into the server profile.
    fn run_tunnel_actions(&mut self, actions: Vec<TunnelAction>, server: Option<Server>) {
        let lang = self.lang();
        for a in actions {
            match a {
                TunnelAction::Add(ctrl, spec) => self.ctrl_add(ctrl, spec, lang),
                TunnelAction::Remove(ctrl, id) => self.ctrl_remove(ctrl, id, lang),
                TunnelAction::SaveServer(spec) => {
                    let Some(srv) = server.clone() else { continue };
                    let entry = spec.to_entry();
                    if let Some(pos) =
                        self.servers.iter().position(|s| s.name == srv.name)
                    {
                        if !self.servers[pos].tunnels.contains(&entry) {
                            self.servers[pos].tunnels.push(entry);
                            self.persist_servers();
                        }
                    }
                    self.toast(tpl(tr(lang, "tnl_saved"), &[("name", &srv.name)]));
                }
            }
        }
    }

    /// Spawn an async add-forward; the outcome lands in the feedback channel.
    fn ctrl_add(&self, ctrl: tunnel::SharedController, spec: ForwardSpec, lang: Language) {
        let tx = self.ctrl_fb_tx.clone();
        self.rt.spawn(async move {
            let msg = match ctrl.add_forward(spec.clone()).await {
                Ok(desc) => tpl(tr(lang, "tnl_added"), &[("line", &desc)]),
                Err(e) => tpl(tr(lang, "tnl_op_fail"), &[("e", &e)]),
            };
            let _ = tx.send(msg);
        });
    }

    /// Spawn an async remove-forward; the outcome lands in the feedback channel.
    fn ctrl_remove(&self, ctrl: tunnel::SharedController, id: u64, lang: Language) {
        let tx = self.ctrl_fb_tx.clone();
        self.rt.spawn(async move {
            let msg = match ctrl.remove_forward(id).await {
                Ok(desc) => tpl(tr(lang, "tnl_removed"), &[("line", &desc)]),
                Err(e) => tpl(tr(lang, "tnl_op_fail"), &[("e", &e)]),
            };
            let _ = tx.send(msg);
        });
    }


    /// resolve_opts + the headless-test TOFU override (same as spawn_connect_tab)
    fn base_opts(&self, server: &Server) -> ConnectOpts {
        let mut opts = resolve_opts(server, &self.settings);
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
        opts
    }

    fn spawn_tunnel_tab(&mut self, server: Server, forwards: Option<Vec<ForwardSpec>>) {
        let forwards = match forwards {
            Some(f) if !f.is_empty() => f,
            Some(_) => {
                self.toast(tr(self.lang(), "tnl_none").to_string());
                return;
            }
            None => match tunnel::parse_tunnel_list(&server.tunnels) {
                Ok(f) if !f.is_empty() => f,
                Ok(_) => {
                    self.toast(tr(self.lang(), "tnl_none").to_string());
                    return;
                }
                Err(e) => {
                    self.toast(tpl(tr(self.lang(), "err_tunnel"), &[("e", &e)]));
                    return;
                }
            },
        };
        let opts = self.base_opts(&server);
        let name = tpl(tr(self.lang(), "tnl_tab"), &[("name", &server.name)]);
        let status = tpl(
            tr(self.lang(), "status_connecting"),
            &[("host", &server.host), ("port", &server.port.to_string())],
        );
        let (result_rx, request_rx) =
            session::spawn_tunnels(&self.rt, server.clone(), opts, forwards);
        self.tabs.push(Tab::TunnelConnecting {
            name,
            result_rx,
            request_rx,
            status,
            server,
        });
        self.active_tab = self.tabs.len() - 1;
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
        let forwards = match self.forwards_for(&server) {
            Ok(f) => f,
            Err(e) => {
                let msg = tpl(tr(self.lang(), "err_tunnel"), &[("e", &e)]);
                self.tabs.push(Tab::Failed {
                    name: server.name.clone(),
                    error: msg,
                    server: Some(server),
                });
                self.active_tab = self.tabs.len() - 1;
                return;
            }
        };
        let name = server.name.clone();
        let status = tpl(
            tr(self.lang(), "status_connecting"),
            &[("host", &server.host), ("port", &server.port.to_string())],
        );
        log::info!("connect_server: spawning");
        let (cols, rows) = self.last_grid;
        let (result_rx, request_rx) =
            session::spawn_connect(&self.rt, server.clone(), opts, cols, rows, forwards);
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
            Some(Tab::Tunnels { server, .. }) => server.clone(),
            _ => return,
        };
        // Tunnels-only tab: tear down and respawn the tunnel session in place
        if matches!(self.tabs.get(idx), Some(Tab::Tunnels { .. })) {
            let old = std::mem::replace(
                &mut self.tabs[idx],
                Tab::Failed { name: String::new(), error: String::new(), server: None },
            );
            if let Tab::Tunnels { close_tx, .. } = old {
                let _ = close_tx.send(());
            }
            let name = tpl(tr(self.lang(), "tnl_tab"), &[("name", &server.name)]);
            match tunnel::parse_tunnel_list(&server.tunnels) {
                Ok(f) if !f.is_empty() => {
                    let opts = self.base_opts(&server);
                    let status = tpl(
                        tr(self.lang(), "status_connecting"),
                        &[("host", &server.host), ("port", &server.port.to_string())],
                    );
                    let (result_rx, request_rx) =
                        session::spawn_tunnels(&self.rt, server.clone(), opts, f);
                    self.tabs[idx] = Tab::TunnelConnecting {
                        name,
                        result_rx,
                        request_rx,
                        status,
                        server,
                    };
                }
                Ok(_) => {
                    self.tabs[idx] = Tab::Failed { name, error: tr(self.lang(), "tnl_none").to_string(), server: Some(server) };
                }
                Err(e) => {
                    let msg = tpl(tr(self.lang(), "err_tunnel"), &[("e", &e)]);
                    self.tabs[idx] = Tab::Failed { name, error: msg, server: Some(server) };
                }
            }
            self.active_tab = idx;
            return;
        }
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
        // tear down whatever session / upload channel occupies this slot
        // (reconnect normally targets a closed tab, but a live one must not
        // leak its SSH session or lazy SFTP channel)
        let old = std::mem::replace(
            &mut self.tabs[idx],
            Tab::Failed { name: String::new(), error: String::new(), server: None },
        );
        if let Tab::Open { handle, mut up, .. } = old {
            up.shutdown();
            let _ = handle.close_tx.send(());
        }
        let forwards = match self.forwards_for(&server) {
            Ok(f) => f,
            Err(e) => {
                let msg = tpl(tr(self.lang(), "err_tunnel"), &[("e", &e)]);
                self.tabs[idx] = Tab::Failed { name: server.name.clone(), error: msg, server: Some(server) };
                self.active_tab = idx;
                return;
            }
        };
        let (cols, rows) = self.last_grid;
        let (result_rx, request_rx) =
            session::spawn_connect(&self.rt, server.clone(), opts, cols, rows, forwards);
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
                    up: TermUp::new(self.lang(), true),
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
            Tab::Open { handle, mut up, .. } => {
                up.shutdown();
                let _ = handle.close_tx.send(());
            }
            Tab::Sftp { st, .. } => {
                st.cleanup();
                let _ = st.close_tx.send(());
            }
            Tab::Tunnels { close_tx, .. } => {
                let _ = close_tx.send(());
            }
            _ => {}
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
    }

    fn has_open_session(&self) -> bool {
        self.tabs.iter().any(|t| {
            matches!(
                t,
                Tab::Open { closed: None, .. } | Tab::Sftp { .. } | Tab::Tunnels { closed: None, .. }
            )
        })
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

        // 1a-bis. Tunnels-only connecting: auth questions (same dialog machinery)
        for i in 0..self.tabs.len() {
            if let Tab::TunnelConnecting { request_rx, .. } = &mut self.tabs[i] {
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

        // 1c. terminal-tab upload channel: auth questions while connecting
        for i in 0..self.tabs.len() {
            if let Tab::Open { up, .. } = &mut self.tabs[i] {
                if self.question.is_none() {
                    if let Some(req) = up.poll_request() {
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
                        server: Some(server.clone()),
                        up: TermUp::new(lang, false),
                    };
                }
                Err(e) => {
                    let msg = e.message(lang);
                    self.tabs[i] = Tab::Failed { name, error: msg, server: Some(server) };
                }
            }
        }

        // 2a-bis. Tunnels-only connect results
        for i in 0..self.tabs.len() {
            let res = if let Tab::TunnelConnecting { result_rx, .. } = &mut self.tabs[i] {
                match result_rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(oneshot::error::TryRecvError::Closed) => {
                        Some(Err(ConnectError::Other("connect task crashed (see log)".into())))
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            if let Some(res) = res {
                let (name, server) = match self.tabs.get(i) {
                    Some(Tab::TunnelConnecting { name, server, .. }) => (name.clone(), server.clone()),
                    _ => continue,
                };
                match res {
                    Ok(handle) => {
                        self.tabs[i] = Tab::Tunnels {
                            name,
                            controller: handle.controller,
                            event_rx: handle.event_rx,
                            closed: None,
                            close_tx: handle.close_tx,
                            server,
                        };
                    }
                    Err(e) => {
                        let msg = e.message(lang);
                        self.tabs[i] = Tab::Failed { name, error: msg, server: Some(server) };
                    }
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

        // 2c. terminal-tab upload channel: connect results
        for i in 0..self.tabs.len() {
            if let Tab::Open { up, .. } = &mut self.tabs[i] {
                if let Some(res) = up.poll_result() {
                    up.on_connect_result(&self.rt, res);
                }
            }
        }

        // 3. Drain PTY output / titles / bells / close events of open tabs
        let mut forwards_up: Vec<(usize, Vec<String>)> = Vec::new();
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
                    self.last_output = Some(std::time::Instant::now());
                }
                while let Some(title) = title_rx.try_recv().ok() {
                    term.title = title;
                }
                if bell_rx.try_recv().is_ok() {
                    term.bell = true;
                }
                if let Some(ev) = handle.event_rx.try_recv().ok() {
                    match ev {
                        SessionEvent::Closed { exit_code, reason } => {
                            log::info!("session closed: {reason} (exit {exit_code:?})");
                            *closed = Some(match exit_code {
                                Some(c) => format!("{reason} (exit {c})"),
                                None => reason,
                            });
                        }
                        SessionEvent::ForwardsUp(lines) => {
                            forwards_up.push((i, lines));
                        }
                    }
                }
                if term.bell {
                    term.bell = false;
                    *bell_flash = true;
                }
            }
        }
        for (_, lines) in forwards_up {
            self.toast(lines.join("  "));
        }

        // 4. Tunnel tabs: lifecycle events
        for i in 0..self.tabs.len() {
            let ev = if let Tab::Tunnels { event_rx, .. } = &mut self.tabs[i] {
                event_rx.try_recv().ok()
            } else {
                None
            };
            if let Some(SessionEvent::Closed { reason, .. }) = ev {
                if let Some(Tab::Tunnels { closed, .. }) = self.tabs.get_mut(i) {
                    *closed = Some(reason);
                }
            }
        }

        // 5. Async tunnel op results (add/remove from the panel/popup)
        while let Ok(msg) = self.ctrl_fb_rx.try_recv() {
            self.toast(msg);
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
                    // Reserve fixed slots for the three square buttons (⚡ / ⛓ / ☰)
                    // plus inter-item spacing, so the row can never overflow the
                    // panel no matter what width is available.
                    let small_w = 26.0;
                    let spacing = ui.style().spacing.item_spacing.x;
                    let conn_w = (ui.available_width() - 3.0 * small_w - 3.0 * spacing - 8.0).max(60.0);
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
                    let tnl_btn = ui
                        .add_sized([small_w, 26.0], egui::Button::new("\u{26d3}"))
                        .on_hover_text(tpl(tr(lang, "qt_title"), &[]));
                    if tnl_btn.clicked() {
                        let sel = self
                            .selected_server
                            .min(self.servers.len().saturating_sub(1));
                        self.quick_tnl =
                            Some(QuickTunnelForm::new(self.servers.len(), sel, None));
                        self.quick_tnl_open = true;
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
                let mut host = format!("{}@{}:{}", self.servers[i].username, self.servers[i].host, self.servers[i].port);
                if !self.servers[i].tunnels.is_empty() {
                    // visible cue that connecting this server also brings up tunnels
                    host.push_str(&format!("  [T x{}]", self.servers[i].tunnels.len()));
                }
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
                    if ui.button(tpl(tr(self.lang(), "menu_quick_tunnel"), &[])).clicked() {
                        let pre = self.servers[i].tunnels.join("\n");
                        self.quick_tnl = Some(QuickTunnelForm::new(
                            self.servers.len(),
                            i,
                            Some(pre),
                        ));
                        self.quick_tnl_open = true;
                        ui.close();
                    }
                    let tunnels_item = ui.add_enabled(
                        !self.servers[i].tunnels.is_empty(),
                        egui::Button::new(tpl(tr(self.lang(), "menu_tunnels"), &[])),
                    );
                    if tunnels_item.clicked() {
                        self.connect_server_tunnels(i);
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
            let mut open_tnl_popup: Option<usize> = None;
            for i in 0..self.tabs.len() {
                let is_active = i == self.active_tab;
                let mut title = self.tabs[i].name().to_string();
                if let Tab::Open { handle, .. } = &self.tabs[i] {
                    if let Some(ctrl) = &handle.tunnel {
                        let n = ctrl.list().len();
                        if n > 0 {
                            title.push_str(&format!(" [T{n}]"));
                        }
                    }
                }
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
                    let has_ctrl =
                        matches!(&self.tabs[i], Tab::Open { handle, .. } if handle.tunnel.is_some());
                    if has_ctrl
                        && ui.button(tpl(tr(self.lang(), "menu_tunnels_manage"), &[])).clicked()
                    {
                        open_tnl_popup = Some(i);
                        ui.close();
                    }
                    if matches!(self.tabs[i], Tab::Open { .. })
                        && ui.button(tpl(tr(self.lang(), "menu_clear"), &[])).clicked()
                    {
                        clear = Some(i);
                        ui.close();
                    }
                    if matches!(self.tabs[i], Tab::Open { .. })
                        && ui.button(tpl(tr(self.lang(), "menu_copy_buffer"), &[])).clicked()
                    {
                        if let Some(Tab::Open { term, .. }) = self.tabs.get(i) {
                            let text = term.buffer_text();
                            if !text.is_empty() {
                                ui.ctx().copy_text(text);
                            }
                        }
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
            if let Some(i) = open_tnl_popup {
                self.open_tunnel_popup(i);
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
        let lang = self.lang();
        let mut do_reconnect = false;
        let mut do_close = false;
        let mut up_note: Option<String> = None;
        let mut tunnel_actions: Vec<TunnelAction> = Vec::new();
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
            Tab::TunnelConnecting { status, .. } => {
                ui.centered_and_justified(|ui| {
                    ui.label(status.clone());
                    ui.spinner();
                });
            }
            Tab::Tunnels { controller, closed, server, .. } => {
                if let Some(reason) = closed.clone() {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(48.0);
                            ui.label(tpl(tr(lang, "status_closed"), &[("reason", &reason)]));
                            ui.add_space(10.0);
                            if ui.button(tpl(tr(lang, "btn_reconnect"), &[])).clicked() {
                                do_reconnect = true;
                            }
                            if ui.button(tpl(tr(lang, "btn_close_tab"), &[])).clicked() {
                                do_close = true;
                            }
                        });
                    });
                } else {
                    ui.add_space(12.0);
                    ui.heading(tpl(tr(lang, "tnl_title"), &[("name", server.name.as_str())]));
                    ui.separator();
                    tunnel_panel_ui(
                        ui,
                        lang,
                        "tnl_tab",
                        controller,
                        &mut self.tunnel_add_form,
                        Some(server),
                        &mut tunnel_actions,
                    );
                }
            }
            Tab::Open { term, handle, resize_sent, closed, bell_flash, server, up, .. } => {
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

                    // ---- drag & drop upload ------------------------------------
                    // OS files dropped on the terminal are copied/uploaded
                    // into the terminal's current directory when the shell
                    // reliably reported it (OSC 7 remote / OSC 9;9 local +
                    // host + freshness); otherwise a directory picker asks.
                    let dropped: Vec<PathBuf> = ui.input(|i| {
                        i.raw
                            .dropped_files
                            .iter()
                            .map(|f| f.path().to_path_buf())
                            .collect()
                    });
                    if !dropped.is_empty() {
                        up.enqueue(dropped);
                    }
                    up.update();
                    if let Some(msg) = up.take_failure() {
                        up_note = Some(msg);
                    }
                    if !up.pending.is_empty() && up.pick.is_none() {
                        if server.is_some() {
                            // drops waiting + no channel yet: open it lazily
                            // (auth questions use the shared question dialog)
                            if matches!(up.state, UpState::Idle) {
                                if let Some(srv) = server {
                                    let opts = resolve_opts(srv, &self.settings);
                                    up.connect(&self.rt, srv.clone(), opts);
                                }
                            }
                            if up.is_ready() {
                                let paths = std::mem::take(&mut up.pending);
                                let host =
                                    server.as_ref().map(|s| s.host.as_str()).unwrap_or_default();
                                match term.trusted_cwd(host) {
                                    Some(dir) => {
                                        let msg =
                                            tpl(tr(up.lang, "up_direct"), &[("dir", &dir)]);
                                        up.note(msg);
                                        up.upload_to(&self.rt, &dir, paths);
                                    }
                                    None => {
                                        let anchor = up.home.clone();
                                        up.pick_open(&self.rt, paths, anchor);
                                    }
                                }
                            }
                        } else {
                            // local tab: copy when the shell's cwd is CERTAIN
                            // (OSC 9;9 hook + COMPUTERNAME match + the dir
                            // really exists); otherwise ask, anchored at the
                            // user profile. The existence check is also the
                            // last guard against a wrong-machine path (e.g. a
                            // nested ssh/wsl whose report slipped through).
                            let paths = std::mem::take(&mut up.pending);
                            let host = local_hostname();
                            match term.trusted_cwd(&host) {
                                Some(dir) if std::path::Path::new(&dir).is_dir() => {
                                    let msg =
                                        tpl(tr(up.lang, "up_copy_dir"), &[("dir", &dir)]);
                                    up.note(msg);
                                    up.upload_to(&self.rt, &dir, paths);
                                }
                                _ => {
                                    let anchor = dirs::home_dir();
                                    up.pick_open_local(&self.rt, paths, anchor);
                                }
                            }
                        }
                    }
                    if let Some((dir, paths)) = up.poll_pick() {
                        up.upload_to(&self.rt, &dir, paths);
                    }
                    let hover = ui.input(|i| !i.raw.hovered_files.is_empty());
                    let area = ui.max_rect();
                    crate::termup::overlay_ui(up, ui, area, hover);
                    if up.transfer_ask.is_some() {
                        let mut ask = up.transfer_ask.take();
                        crate::termup::conflict_ask_ui(ui, up.lang, &mut ask);
                        up.transfer_ask = ask;
                    }
                    up.pick_ui(ui, &self.rt);
                }
            }
        }
        if do_close {
            self.close_tab(i);
        } else if do_reconnect {
            self.reconnect_tab(i);
        }
        if !tunnel_actions.is_empty() {
            let server = match self.tabs.get(i) {
                Some(Tab::Tunnels { server, .. }) => Some(server.clone()),
                _ => None,
            };
            self.run_tunnel_actions(tunnel_actions, server);
        }
        if let Some(note) = up_note {
            self.toast(note);
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
                        ui.label(tr(lang, "f_tunnels"));
                        ui.add(
                            egui::TextEdit::multiline(&mut form.tunnels)
                                .hint_text(tr(lang, "f_tunnels_hint"))
                                .desired_width(220.0),
                        );
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
                } else if let Err(e) = tunnel::parse_tunnel_list(&built.tunnels) {
                    self.form.as_mut().unwrap().error =
                        Some(tpl(tr(lang, "err_tunnel"), &[("e", &e)]));
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
        // Quick-tunnels dialog (ad-hoc forwards; this-time-only or save to server)
        if self.quick_tnl.is_some() {
            let mut open = self.quick_tnl_open;
            let mut once = false;
            let mut save = false;
            let mut cancel = false;
            let lang = dlg_lang;
            let n_servers = self.servers.len();
            let win = egui::Window::new(tr(lang, "qt_title"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false);
            win.show(ui, |ui| {
                let qt = self.quick_tnl.as_mut().unwrap();
                egui::Grid::new("quick_tnl_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(tr(lang, "qt_server"));
                        let sel_text = if qt.sel < n_servers {
                            let srv = &self.servers[qt.sel];
                            format!(
                                "{} — {}@{}:{}",
                                srv.name, srv.username, srv.host, srv.port
                            )
                        } else {
                            tr(lang, "qt_temp").to_string()
                        };
                        egui::ComboBox::from_id_salt("quick_tnl_srv")
                            .selected_text(sel_text)
                            .show_ui(ui, |ui| {
                                for (i, srv) in self.servers.iter().enumerate() {
                                    let label = format!(
                                        "{} — {}@{}:{}",
                                        srv.name, srv.username, srv.host, srv.port
                                    );
                                    ui.selectable_value(&mut qt.sel, i, label);
                                }
                                ui.selectable_value(
                                    &mut qt.sel,
                                    n_servers,
                                    tr(lang, "qt_temp"),
                                );
                            });
                        ui.end_row();
                        if qt.sel == n_servers {
                            ui.label(tr(lang, "f_host"));
                            ui.add(
                                egui::TextEdit::singleline(&mut qt.temp_host)
                                    .hint_text("example.com")
                                    .desired_width(220.0),
                            );
                            ui.end_row();
                            ui.label(tr(lang, "f_port"));
                            ui.add(
                                egui::TextEdit::singleline(&mut qt.temp_port)
                                    .desired_width(80.0),
                            );
                            ui.end_row();
                            ui.label(tr(lang, "f_user"));
                            ui.text_edit_singleline(&mut qt.temp_user);
                            ui.end_row();
                            ui.label(tr(lang, "f_password"));
                            ui.add(
                                egui::TextEdit::singleline(&mut qt.temp_password)
                                    .password(true),
                            );
                            ui.end_row();
                        }
                        ui.label(tr(lang, "f_tunnels"));
                        ui.add(
                            egui::TextEdit::multiline(&mut qt.text)
                                .hint_text(tr(lang, "f_tunnels_hint"))
                                .desired_width(280.0),
                        );
                        ui.end_row();
                    });
                if let Some(err) = &qt.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err.clone());
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(tpl(tr(lang, "qt_once"), &[])).clicked() {
                        once = true;
                    }
                    if ui
                        .add_enabled(
                            qt.sel < n_servers,
                            egui::Button::new(tpl(tr(lang, "qt_save"), &[])),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.button(tr(lang, "btn_cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
            self.quick_tnl_open = open;
            if once || save {
                // pull the editable data out of self before mutating anything
                let (text, sel, temp) = match self.quick_tnl.as_ref() {
                    Some(qt) => (qt.text.clone(), qt.sel, Some(qt.build_temp_server())),
                    None => (String::new(), 0, None),
                };
                let mut parsed: Vec<ForwardSpec> = Vec::new();
                let mut bad: Option<String> = None;
                for (n, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match tunnel::parse_tunnel_entry(line) {
                        Ok(f) => parsed.push(f),
                        Err(e) => {
                            bad = Some(tpl(
                                tr(lang, "qt_line_err"),
                                &[("n", &(n + 1).to_string()), ("e", &e)],
                            ));
                            break;
                        }
                    }
                }
                if parsed.is_empty() && bad.is_none() {
                    bad = Some(tr(lang, "tnl_none").to_string());
                }
                if bad.is_none() && sel >= n_servers {
                    // temporary target: a host is mandatory
                    if text_host_empty(&temp) {
                        bad = Some(tr(lang, "err_host_required").to_string());
                    }
                }
                if let Some(err) = bad {
                    if let Some(qt) = self.quick_tnl.as_mut() {
                        qt.error = Some(err);
                    }
                } else {
                    let mut server =
                        if sel < n_servers { self.servers[sel].clone() } else { temp.unwrap() };
                    if save && sel < n_servers {
                        // merge (dedupe by parsed rule, not by string form)
                        let mut existing =
                            tunnel::parse_tunnel_list(&server.tunnels).unwrap_or_default();
                        for f in &parsed {
                            if !existing.contains(f) {
                                existing.push(f.clone());
                                server.tunnels.push(f.to_entry());
                            }
                        }
                        if let Some(idx) =
                            self.servers.iter().position(|srv| srv.name == server.name)
                        {
                            self.servers[idx] = server.clone();
                        }
                        self.persist_servers();
                    }
                    self.quick_tnl = None;
                    self.quick_tnl_open = false;
                    self.spawn_tunnel_tab(server, Some(parsed));
                }
            }
            if cancel || !open {
                self.quick_tnl = None;
                self.quick_tnl_open = false;
            }
        }

        // Tunnel manager popup (terminal tab "Tunnels..."): shared panel bound
        // to one live session; close = hide only, the session keeps running.
        if self.tunnel_popup.is_some() {
            let mut close = false;
            let lang = dlg_lang;
            if let Some(tp) = &mut self.tunnel_popup {
                let mut open = true;
                egui::Window::new(tp.title.clone())
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(true)
                    .default_width(560.0)
                    .show(ui, |ui| {
                        let mut acts: Vec<TunnelAction> = Vec::new();
                        tunnel_panel_ui(
                            ui,
                            lang,
                            "tnl_popup",
                            &tp.controller,
                            &mut tp.add,
                            tp.server.as_ref(),
                            &mut acts,
                        );
                        tp.actions.extend(acts);
                    });
                if !open {
                    close = true;
                }
            }
            let acts = self
                .tunnel_popup
                .as_mut()
                .map(|tp| std::mem::take(&mut tp.actions))
                .unwrap_or_default();
            if !acts.is_empty() {
                let server = self.tunnel_popup.as_ref().and_then(|tp| tp.server.clone());
                self.run_tunnel_actions(acts, server);
            }
            if close {
                self.tunnel_popup = None;
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
        // Heartbeat is adaptive: fast (16ms) while output has been flowing in
        // the last 300ms so bursts render smoothly, otherwise a slow 100ms
        // poll that merely watches the channels — idle CPU drops to near zero
        // and input events still trigger immediate repaints on their own.
        self.poll_tabs();
        let live = self.tabs.iter().any(|t| {
            matches!(
                t,
                Tab::Connecting { .. }
                    | Tab::Open { closed: None, .. }
                    | Tab::TunnelConnecting { .. }
                    | Tab::Tunnels { closed: None, .. }
            )
        });
        if live {
            let recent_output = self
                .last_output
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300));
            let period = if recent_output { 16 } else { 100 };
            ctx.request_repaint_after(std::time::Duration::from_millis(period));
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
