//! SSH session layer for the GUI, decoupled from any UI toolkit.
//!
//! Modeled on xxssh 0.5.2 `src/ssh.rs` (connect/auth/keepalive/PTY logic), reorganized
//! as a spawned tokio task that talks to the GUI through channels:
//!
//! - GUI -> session: keyboard input bytes, PTY resize, connect-time answers
//!   (password prompt, host-key confirmation) delivered as `ConnectRequest`s
//!   the UI polls each frame and answers via oneshot channels.
//! - session -> GUI: raw PTY output bytes (ANSI pass-through) and `SessionEvent`s
//!   (closed / keepalive timeout).
//!
//! Keepalive matches xxssh exactly: every 20s, dead after 3 unanswered.
//! PTY modes match xxssh: only OPOST/ONLCR/CS8 (ICANON would eat arrow keys).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{ssh_key, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect, Pty};
use tokio::sync::{mpsc, oneshot};

use crate::i18n::{self, Language};
use crate::xconfig::{AuthMethod, Server};

/// Keepalive: send a keepalive every 20s; consider connection dead after 3 unanswered
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_MAX: usize = 3;

/// Default TCP+handshake timeout (same as xxssh)
const CONNECT_TIMEOUT_DEFAULT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Connection-level options (resolved by the caller: server field > global settings)
#[derive(Clone, Default)]
pub struct ConnectOpts {
    /// Resolved SOCKS5 proxy URL, if any
    pub proxy: Option<String>,
    /// TCP+SSH handshake timeout seconds (None = 30s default, Some(0) = unlimited)
    pub connect_timeout: Option<u64>,
    /// Strict known_hosts file: unknown or mismatched host keys are rejected
    pub known_hosts: Option<String>,
    /// TOFU known_hosts file: unknown hosts are learned; mismatches rejected
    pub known_hosts_add: Option<String>,
    /// GUI TOFU store (~/.xxssh/known_hosts): unknown keys trigger an interactive
    /// confirm dialog, accepted keys are persisted so later connects skip the dialog.
    pub host_keystore: Option<String>,
}

/// A question the session task asks the UI while connecting; the UI polls these
/// each frame, shows a dialog, and answers through the oneshot responder.
pub enum ConnectRequest {
    /// Unknown (or CHANGED when `changed`) host key — UI shows the fingerprint and
    /// answers true (trust) / false (reject)
    HostKey {
        host: String,
        port: u16,
        fingerprint: String,
        changed: bool,
        respond: oneshot::Sender<bool>,
    },
    /// Password needed (none stored) — UI shows a password box; None = cancel
    Password {
        user: String,
        host: String,
        respond: oneshot::Sender<Option<String>>,
    },
    /// Private key passphrase needed — None = cancel
    Passphrase {
        path: String,
        respond: oneshot::Sender<Option<String>>,
    },
}

/// Session lifecycle events (after the shell is open)
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Connection dead (EOF, keepalive timeout, error). exit_code if the remote sent one.
    Closed { exit_code: Option<u32>, reason: String },
}

/// Failure kinds, so the UI can show a localized message per kind
#[derive(Debug, Clone)]
pub enum ConnectError {
    Timeout { host: String, secs: u64 },
    Network(String),
    Proxy(String),
    Auth(String),
    #[allow(dead_code)] // reserved: set when strict known_hosts rejection surfaces as its own dialog
    HostKeyRejected(String),
    Cancelled,
    Other(String),
}

impl ConnectError {
    /// Localized one-line message
    pub fn message(&self, lang: Language) -> String {
        match self {
            ConnectError::Timeout { host, secs } => i18n::tpl(
                i18n::tr(lang, "status_connect_timeout"),
                &[("host", host), ("secs", &secs.to_string())],
            ),
            ConnectError::Network(e) => {
                i18n::tpl(i18n::tr(lang, "status_conn_fail"), &[("e", e)])
            }
            ConnectError::Proxy(e) => i18n::tpl(i18n::tr(lang, "status_proxy_fail"), &[("e", e)]),
            ConnectError::Auth(e) => e.clone(),
            ConnectError::HostKeyRejected(e) => e.clone(),
            ConnectError::Cancelled => i18n::tr(lang, "status_cancelled").to_string(),
            ConnectError::Other(e) => e.clone(),
        }
    }
}

/// Live PTY channel endpoints handed to the GUI once the shell is open.
/// The session task owns the russh channel; the GUI only talks through these.
pub struct SessionHandle {
    /// Keyboard input bytes (raw; the remote line discipline handles editing)
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Resize the remote PTY: (cols, rows)
    pub resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    /// Raw PTY output (ANSI sequences included) for the terminal emulator
    pub output_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Lifecycle events
    pub event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    /// Force-disconnect (equivalent of `~.`)
    pub close_tx: oneshot::Sender<()>,
}

// ---------------------------------------------------------------------------
// Spawn entry point
// ---------------------------------------------------------------------------

/// Spawn the connect+shell task on the shared tokio runtime. Returns:
/// - a receiver for the connect outcome (`Ok(handle)` / `Err(ConnectError)`),
/// - a receiver for connect-time questions (`ConnectRequest`) the UI must answer.
///
/// On success the task has already opened the PTY+shell at `cols`x`rows`.
pub fn spawn_connect(
    rt: &tokio::runtime::Handle,
    server: Server,
    opts: ConnectOpts,
    lang: Language,
    cols: u16,
    rows: u16,
) -> (
    oneshot::Receiver<Result<SessionHandle, ConnectError>>,
    mpsc::UnboundedReceiver<ConnectRequest>,
) {
    let (result_tx, result_rx) = oneshot::channel();
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    log::info!("spawn_connect: task spawning for {host}", host = server.host);
    rt.spawn(async move {
        let res = connect_and_open(server, opts, lang, cols, rows, req_tx).await;
        log::info!("spawn_connect: finished, ok={}", res.is_ok());
        let _ = result_tx.send(res);
    });
    (result_rx, req_rx)
}

/// Owned-data SOCKS5 connect so the boxed future is Send + 'static
async fn socks_connect(
    addr: String,
    target: (String, u16),
    username: Option<String>,
    password: Option<String>,
) -> Result<tokio_socks::tcp::Socks5Stream<tokio::net::TcpStream>, tokio_socks::Error> {
    if let (Some(u), Some(pw)) = (username.as_deref(), password.as_deref()) {
        tokio_socks::tcp::Socks5Stream::connect_with_password(addr.as_str(), target, u, pw).await
    } else {
        tokio_socks::tcp::Socks5Stream::connect(addr.as_str(), target).await
    }
}

async fn connect_and_open(
    server: Server,
    opts: ConnectOpts,
    lang: Language,
    cols: u16,
    rows: u16,
    requests: mpsc::UnboundedSender<ConnectRequest>,
) -> Result<SessionHandle, ConnectError> {
    let limit = match opts.connect_timeout {
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(s)),
        None => Some(Duration::from_secs(CONNECT_TIMEOUT_DEFAULT_SECS)),
    };

    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    });

    // Transport: SOCKS5 or direct (ProxyStream logic from xxssh)
    log::debug!("connect_and_open: begin, proxy={:?}", opts.proxy);
    let stream: ProxyStream = match opts.proxy.as_deref() {
        Some(url) => {
            let p = parse_proxy_url(url).map_err(ConnectError::Proxy)?;
            let target = (server.host.clone(), server.port);
            let fut: std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                tokio_socks::tcp::Socks5Stream<tokio::net::TcpStream>,
                                tokio_socks::Error,
                            >,
                        > + Send
                        + 'static,
                >,
            > = Box::pin(socks_connect(
                p.addr.clone(),
                target,
                p.username.clone(),
                p.password.clone(),
            ));
            let s = match limit {
                Some(d) => match tokio::time::timeout(d, fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        return Err(ConnectError::Timeout {
                            host: server.host.clone(),
                            secs: limit.map(|x| x.as_secs()).unwrap_or(0),
                        })
                    }
                },
                None => fut.await,
            };
            s.map(ProxyStream::Socks)
                .map_err(|e| ConnectError::Proxy(e.to_string()))?
        }
        None => {
            let fut = tokio::net::TcpStream::connect((server.host.as_str(), server.port));
            let tcp = match limit {
                Some(d) => match tokio::time::timeout(d, fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        return Err(ConnectError::Timeout {
                            host: server.host.clone(),
                            secs: limit.map(|x| x.as_secs()).unwrap_or(0),
                        })
                    }
                },
                None => fut.await,
            };
            match tcp {
                Ok(tcp) => {
                    let _ = tcp.set_nodelay(true);
                    ProxyStream::Direct(tcp)
                }
                Err(e) => return Err(ConnectError::Network(e.to_string())),
            }
        }
    };

    let handler = Handler {
        host: server.host.clone(),
        port: server.port,
        policy_strict: opts.known_hosts.clone(),
        policy_learn: opts.known_hosts_add.clone(),
        keystore: opts.host_keystore.clone(),
        requests: requests.clone(),
        approved: HashSet::new(),
        reject_reason: None,
    };

    let handshake = client::connect_stream(config, stream, handler);
    let mut session = match limit {
        Some(d) => match tokio::time::timeout(d, handshake).await {
            Ok(r) => r,
            Err(_) => {
                return Err(ConnectError::Timeout {
                    host: server.host.clone(),
                    secs: limit.map(|x| x.as_secs()).unwrap_or(0),
                })
            }
        },
        None => handshake.await,
    }
    .map_err(|e| ConnectError::Network(e.to_string()))?;

    log::debug!("connect_and_open: handshake done, authenticating ({:?})", server.auth);
    // Authenticate
    let auth = match server.auth {
        AuthMethod::Password => auth_password(&mut session, &server, &requests, lang).await,
        AuthMethod::Key => auth_key(&mut session, &server, &requests).await,
    };
    if let Err(e) = auth {
        let _ = session
            .disconnect(Disconnect::ByApplication, "auth failed", "en")
            .await;
        return Err(match e {
            AuthErr::Cancelled => ConnectError::Cancelled,
            AuthErr::Failed(msg) => ConnectError::Auth(msg),
        });
    }

    log::debug!("connect_and_open: auth ok, opening PTY+shell");
    // Open channel + PTY + shell (PTY modes copied from xxssh)
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| ConnectError::Other(e.to_string()))?;
    let modes: &[(Pty, u32)] = &[(Pty::OPOST, 1), (Pty::ONLCR, 1), (Pty::CS8, 1)];
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, modes)
        .await
        .map_err(|e| ConnectError::Other(e.to_string()))?;
    channel
        .request_shell(true)
        .await
        .map_err(|e| ConnectError::Other(e.to_string()))?;

    // Split endpoints for the GUI
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let (close_tx, mut close_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut exit_code: Option<u32> = None;
        let mut closed = false;
        let mut reason = String::from("eof");
        // EOF may arrive before exit-status (xxssh: wait up to 3s for the status)
        let mut eof_deadline: Option<tokio::time::Instant> = None;

        loop {
            // Pending EOF grace timer; fires only while a deadline is set
            let grace = async {
                match eof_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = grace => {
                    closed = true;
                    reason = "eof".into();
                }
                // keyboard input from GUI
                r = input_rx.recv() => {
                    match r {
                        Some(data) => {
                            if channel.data(&data[..]).await.is_err() {
                                closed = true;
                                reason = "write failed".into();
                            }
                        }
                        None => { /* input side dropped; keep session */ }
                    }
                }
                // PTY resize from GUI
                r = resize_rx.recv() => {
                    if let Some((cols, rows)) = r {
                        let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                }
                // forced close
                _ = &mut close_rx => {
                    let _ = channel.eof().await;
                    let _ = session
                        .disconnect(Disconnect::ByApplication, "closed by user", "en")
                        .await;
                    closed = true;
                    reason = "closed by user".into();
                }
                // remote output
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                            // Raw pass-through (ANSI intact). Bell filtering is a GUI policy.
                            if output_tx.send(data.to_vec()).is_err() {
                                break; // GUI gone
                            }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            exit_code = Some(exit_status);
                            // Status arrived — no need to keep waiting on the EOF grace
                            eof_deadline = None;
                        }
                        Some(ChannelMsg::Eof) => {
                            // Grace: exit-status may arrive after EOF
                            eof_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                        Some(ChannelMsg::Close) => {
                            closed = true;
                            reason = "remote closed".into();
                        }
                        None => {
                            closed = true;
                            reason = "connection lost".into();
                        }
                        Some(_) => {}
                    }
                }
            }
            if closed {
                break;
            }
        }
        let _ = event_tx.send(SessionEvent::Closed { exit_code, reason });
        let _ = session
            .disconnect(Disconnect::ByApplication, "session end", "en")
            .await;
    });

    log::debug!("connect_and_open: shell open, handing endpoints to UI");
    Ok(SessionHandle {
        input_tx,
        resize_tx,
        output_rx,
        event_rx,
        close_tx,
    })
}

// ---------------------------------------------------------------------------
// Auth (logic copied from xxssh; prompts routed to the GUI)
// ---------------------------------------------------------------------------

enum AuthErr {
    Cancelled,
    Failed(String),
}

async fn ask_password(
    requests: &mpsc::UnboundedSender<ConnectRequest>,
    user: &str,
    host: &str,
) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    requests
        .send(ConnectRequest::Password {
            user: user.to_string(),
            host: host.to_string(),
            respond: tx,
        })
        .ok()?;
    rx.await.ok().flatten()
}

async fn auth_password(
    session: &mut client::Handle<Handler>,
    server: &Server,
    requests: &mpsc::UnboundedSender<ConnectRequest>,
    _lang: Language,
) -> Result<(), AuthErr> {
    // Stored password, or ask the user
    let pwd = if server.password.is_empty() {
        match ask_password(requests, &server.username, &server.host).await {
            Some(p) => p,
            None => return Err(AuthErr::Cancelled),
        }
    } else {
        server.password.clone()
    };

    let res = session
        .authenticate_password(&server.username, &pwd)
        .await
        .map_err(|e| AuthErr::Failed(e.to_string()))?;
    if res.success() {
        return Ok(());
    }

    // Fallback: keyboard-interactive with the same password (xxssh behavior)
    match session
        .authenticate_keyboard_interactive_start(&server.username, None)
        .await
    {
        Ok(client::KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. }) => {
            let responses: Vec<String> = prompts.iter().map(|_| pwd.clone()).collect();
            match session
                .authenticate_keyboard_interactive_respond(responses)
                .await
            {
                Ok(client::KeyboardInteractiveAuthResponse::Success) => Ok(()),
                Ok(_) => Err(AuthErr::Failed("password rejected".into())),
                Err(e) => Err(AuthErr::Failed(e.to_string())),
            }
        }
        Ok(_) => Err(AuthErr::Failed("password rejected".into())),
        Err(e) => Err(AuthErr::Failed(e.to_string())),
    }
}

async fn ask_passphrase(
    requests: &mpsc::UnboundedSender<ConnectRequest>,
    path: &str,
) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    requests
        .send(ConnectRequest::Passphrase {
            path: path.to_string(),
            respond: tx,
        })
        .ok()?;
    rx.await.ok().flatten()
}

async fn auth_key(
    session: &mut client::Handle<Handler>,
    server: &Server,
    requests: &mpsc::UnboundedSender<ConnectRequest>,
) -> Result<(), AuthErr> {
    let path = server.key_path.trim();
    if path.is_empty() {
        return Err(AuthErr::Failed("no private key path configured".into()));
    }

    // Load the key; if it is encrypted and no passphrase is stored, ask the GUI once
    let mut passphrase: Option<String> = if server.key_passphrase.is_empty() {
        None
    } else {
        Some(server.key_passphrase.clone())
    };
    let key = match russh::keys::load_secret_key(path, passphrase.as_deref()) {
        Ok(k) => k,
        Err(e) => {
            if passphrase.is_none() {
                // Likely an encrypted key without a stored passphrase — prompt and retry
                passphrase = ask_passphrase(requests, path).await;
                if passphrase.is_none() {
                    return Err(AuthErr::Cancelled);
                }
                russh::keys::load_secret_key(path, passphrase.as_deref())
                    .map_err(|e2| AuthErr::Failed(e2.to_string()))?
            } else {
                return Err(AuthErr::Failed(e.to_string()));
            }
        }
    };

    let rsa_hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| AuthErr::Failed(e.to_string()))?
        .flatten();
    let res = session
        .authenticate_publickey(
            &server.username,
            PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
        )
        .await
        .map_err(|e| AuthErr::Failed(e.to_string()))?;
    if res.success() {
        Ok(())
    } else {
        Err(AuthErr::Failed("public key rejected".into()))
    }
}

// ---------------------------------------------------------------------------
// Host key policy (strict / TOFU-file / interactive confirm), following xxssh
// ---------------------------------------------------------------------------

struct Handler {
    host: String,
    port: u16,
    policy_strict: Option<String>,
    policy_learn: Option<String>,
    /// GUI TOFU store: accepted host keys are persisted here so the confirm
    /// dialog only appears on the first connect (or after a key change)
    keystore: Option<String>,
    requests: mpsc::UnboundedSender<ConnectRequest>,
    /// Fingerprints approved during this connection (only when no keystore)
    approved: HashSet<String>,
    /// Human-readable rejection reason surfaced to the UI on failure
    reject_reason: Option<String>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        let fp = key
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string();

        // Strict known_hosts: unknown or mismatched -> reject
        if let Some(file) = &self.policy_strict {
            return match russh::keys::known_hosts::check_known_hosts_path(
                &self.host,
                self.port,
                key,
                file,
            ) {
                Ok(true) => Ok(true),
                Ok(false) => {
                    self.reject_reason =
                        Some(format!("host key mismatch/not found in strict known_hosts ({fp})"));
                    Ok(false)
                }
                Err(e) => {
                    self.reject_reason = Some(format!("known_hosts check failed: {e}"));
                    Ok(false)
                }
            };
        }

        // TOFU known_hosts: learn unknown keys, reject mismatches
        if let Some(file) = &self.policy_learn {
            return match russh::keys::known_hosts::check_known_hosts_path(
                &self.host,
                self.port,
                key,
                file,
            ) {
                Ok(true) => Ok(true),
                Ok(false) => {
                    match russh::keys::known_hosts::learn_known_hosts_path(
                        &self.host,
                        self.port,
                        key,
                        file,
                    ) {
                        Ok(()) => Ok(true),
                        Err(e) => {
                            self.reject_reason = Some(format!("failed to learn host key: {e}"));
                            Ok(false)
                        }
                    }
                }
                Err(e) => {
                    self.reject_reason = Some(format!("host key mismatch: {e}"));
                    Ok(false)
                }
            };
        }

        // No explicit file policy: persistent TOFU keystore + interactive confirm.
        let mut changed = false;
        if let Some(file) = &self.keystore {
            match russh::keys::known_hosts::check_known_hosts_path(
                &self.host,
                self.port,
                key,
                file,
            ) {
                Ok(true) => return Ok(true),
                Err(russh::keys::Error::KeyChanged { .. }) => changed = true,
                _ => {}
            }
        }
        if !changed && self.approved.contains(&fp) {
            return Ok(true);
        }
        let (tx, rx) = oneshot::channel();
        if self
            .requests
            .send(ConnectRequest::HostKey {
                host: self.host.clone(),
                port: self.port,
                fingerprint: fp.clone(),
                changed,
                respond: tx,
            })
            .is_err()
        {
            return Ok(false);
        }
        match rx.await {
            Ok(true) => {
                if let Some(file) = &self.keystore {
                    if changed {
                        // A different key was recorded for this host: drop the stale
                        // entries before appending, or every later check still fails
                        // with KeyChanged (russh's learn only appends).
                        remove_known_host_entries(file, &self.host, self.port);
                    }
                    if russh::keys::known_hosts::learn_known_hosts_path(
                        &self.host,
                        self.port,
                        key,
                        file,
                    )
                    .is_err()
                    {
                        // Persist failed: still allow this connection
                        self.approved.insert(fp);
                    }
                } else {
                    self.approved.insert(fp);
                }
                Ok(true)
            }
            _ => {
                self.reject_reason = Some(if changed {
                    format!("host key CHANGED for {}:{} ({fp})", self.host, self.port)
                } else {
                    format!("host key rejected by user ({fp})")
                });
                Ok(false)
            }
        }
    }
}

/// Remove all existing known_hosts lines for host:port (used when accepting a
/// changed host key). Line numbers come from russh's `known_host_keys_path`.
fn remove_known_host_entries(file: &str, host: &str, port: u16) {
    let Ok(entries) = russh::keys::known_hosts::known_host_keys_path(host, port, file) else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    let drop_lines: HashSet<usize> = entries.into_iter().map(|(line, _)| line).collect();
    if let Ok(text) = std::fs::read_to_string(file) {
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(n, _)| !drop_lines.contains(&(n + 1)))
            .map(|(_, l)| l)
            .collect();
        let _ = std::fs::write(file, kept.join("
") + "
");
    }
}

// ---------------------------------------------------------------------------
// SOCKS5 proxy plumbing (copied from xxssh)
// ---------------------------------------------------------------------------

/// Parsed SOCKS5 proxy configuration: "socks5://[user:pass@]host:port"
pub struct ProxyConfig {
    pub addr: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub fn parse_proxy_url(url: &str) -> Result<ProxyConfig, String> {
    let s = url.trim();
    let rest = s
        .strip_prefix("socks5://")
        .or_else(|| s.strip_prefix("socks5h://"))
        .unwrap_or(s);
    if rest.is_empty() {
        return Err("empty proxy address".into());
    }
    let (userinfo, hostport) = match rest.rfind('@') {
        Some(idx) => (Some(&rest[..idx]), &rest[idx + 1..]),
        None => (None, rest),
    };
    if !hostport.contains(':') {
        return Err(format!("proxy address missing port: {hostport}"));
    }
    let (username, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((user, pass)) => (Some(user.to_string()), Some(pass.to_string())),
            None => (Some(u.to_string()), None),
        },
        None => (None, None),
    };
    Ok(ProxyConfig { addr: hostport.to_string(), username, password })
}

/// Unified transport stream: direct TCP or SOCKS5-tunneled (from xxssh)
enum ProxyStream {
    Direct(tokio::net::TcpStream),
    Socks(tokio_socks::tcp::Socks5Stream<tokio::net::TcpStream>),
}

impl tokio::io::AsyncRead for ProxyStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Direct(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            ProxyStream::Socks(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ProxyStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ProxyStream::Direct(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            ProxyStream::Socks(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Direct(s) => std::pin::Pin::new(s).poll_flush(cx),
            ProxyStream::Socks(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Direct(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            ProxyStream::Socks(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proxy_roundtrip() {
        let p = parse_proxy_url("socks5://user:pw@127.0.0.1:1080").unwrap();
        assert_eq!(p.addr, "127.0.0.1:1080");
        assert_eq!(p.username.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("pw"));

        let p = parse_proxy_url("127.0.0.1:1080").unwrap();
        assert_eq!(p.addr, "127.0.0.1:1080");
        assert!(p.username.is_none());

        assert!(parse_proxy_url("no-port-here").is_err());
        assert!(parse_proxy_url("socks5://").is_err());
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::xconfig::{AuthMethod, Server};

    /// Real connection test against the xxssh test server (skipped unless
    /// XXSSHG_E2E=1 to keep `cargo test` offline-friendly).
    #[test]
    fn connect_and_open_shell_e2e() {
        if std::env::var("XXSSHG_E2E").unwrap_or_default() != "1" {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = Server {
            name: "test".into(),
            host: "TEST_HOST_REDACTED".into(),
            port: 22,
            username: "root".into(),
            auth: AuthMethod::Password,
            password: "REDACTED".into(),
            key_path: String::new(),
            key_passphrase: String::new(),
            proxy: String::new(),
        };
        let (result_rx, mut req_rx) =
            spawn_connect(rt.handle(), server, ConnectOpts::default(), Language::En, 80, 24);
        // Answer host-key confirms in the background
        let ans = std::thread::spawn(move || {
            loop {
                match req_rx.blocking_recv() {
                    Some(ConnectRequest::HostKey { respond, .. }) => {
                        let _ = respond.send(true);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        });
        let res = rt.block_on(async move {
            // generous overall timeout
            tokio::time::timeout(std::time::Duration::from_secs(30), result_rx).await
        });
        match res {
            Ok(Ok(Ok(mut handle))) => {
                let _ = handle.input_tx.send(b"echo xxsshg_e2e_ok && exit\n".to_vec());
                let mut got_out = false;
                let mut closed = false;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while std::time::Instant::now() < deadline && !(got_out && closed) {
                    rt.block_on(async {
                        tokio::select! {
                            out = handle.output_rx.recv() => {
                                if let Some(bytes) = out {
                                    if std::env::var("XXSSHG_E2E_DUMP").is_ok() {
                                        print!("{}", String::from_utf8_lossy(&bytes));
                                    }
                                    if bytes.windows(13).any(|w| w == b"xxsshg_e2e_ok") {
                                        got_out = true;
                                    }
                                }
                            }
                            ev = handle.event_rx.recv() => {
                                if let Some(SessionEvent::Closed { .. }) = ev {
                                    closed = true;
                                }
                            }
                        }
                    });
                }
                assert!(got_out, "never saw the echo marker in PTY output");
                ans.join().ok();
                println!("E2E OK");
            }
            Ok(Ok(Err(e))) => panic!("connect failed: {e:?}"),
            Ok(Err(_)) => panic!("connect task dropped the result (panicked)"),
            Err(_) => panic!("timed out waiting for connect result"),
        }
    }
}
