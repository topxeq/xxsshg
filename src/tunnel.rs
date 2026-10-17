//! GUI port forwarding (same engine as xxssh): `-D` (local SOCKS5 proxy, targets
//! resolved by the SSH server), `-L` (local listener → remote target via
//! direct-tcpip), `-R` (server-side listener → local target via forwarded-tcpip).
//!
//! Live forwards of one session are managed by a [`TunnelController`]: the UI can
//! list them (sync, cheap), add new ones and remove running ones at any time, and
//! read per-forward connection/traffic counters (`FwdStats`, accumulated from
//! `copy_bidirectional` results).
//!
//! All listeners share the authenticated russh session through an
//! `Arc<tokio::sync::Mutex<Handle>>` (russh `Handle` is not `Clone`); opening a
//! channel takes the lock briefly, so tunnel traffic never blocks the PTY loop.
//! Server-initiated `-R` channels arrive in the client `Handler`
//! (`server_channel_open_forwarded_tcpip`); the handler looks the local target up
//! in a port-keyed table the controller fills when registering the forward.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use russh::client;
use russh::ChannelMsg;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// One port-forwarding rule (parsed from a profile's `tunnels` list or built
/// from UI form parts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardSpec {
    /// `-D [bind:]port` — local SOCKS5 proxy; the host the SOCKS client asks for is
    /// opened on the SSH server (remote DNS, like `ssh -D`)
    Dynamic { bind: String, port: u16 },
    /// `-L [bind:]port:host:rport` — local listener, each connection is a
    /// direct-tcpip channel to `host:rport` as seen from the server
    Local { bind: String, port: u16, host: String, rport: u16 },
    /// `-R [bind:]port:host:rport` — the server listens on `bind:port`, each
    /// connection comes back through a forwarded-tcpip channel and is connected to
    /// `host:rport` as seen from the local machine
    Remote { bind: String, port: u16, host: String, rport: u16 },
}

impl ForwardSpec {
    /// Human-readable one-liner for the "listening" report.
    pub fn describe(&self) -> String {
        match self {
            ForwardSpec::Dynamic { bind, port } => format!("SOCKS5 {bind}:{port}"),
            ForwardSpec::Local { bind, port, host, rport } => {
                format!("{bind}:{port} -> {host}:{rport}")
            }
            ForwardSpec::Remote { bind, port, host, rport } => {
                format!("remote {bind}:{port} -> {host}:{rport}")
            }
        }
    }

    /// Canonical `tunnels`-entry string (the servers.json storage format shared
    /// with the xxssh CLI): bind is omitted when it equals the default.
    pub fn to_entry(&self) -> String {
        match self {
            ForwardSpec::Dynamic { bind, port } => {
                if bind == DEFAULT_BIND {
                    format!("D:{port}")
                } else {
                    format!("D:{bind}:{port}")
                }
            }
            ForwardSpec::Local { bind, port, host, rport } => {
                if bind == DEFAULT_BIND {
                    format!("L:{port}:{host}:{rport}")
                } else {
                    format!("L:{bind}:{port}:{host}:{rport}")
                }
            }
            ForwardSpec::Remote { bind, port, host, rport } => {
                if bind.is_empty() {
                    format!("R:{port}:{host}:{rport}")
                } else {
                    format!("R:{bind}:{port}:{host}:{rport}")
                }
            }
        }
    }
}

/// Default local bind address (matches OpenSSH: loopback unless told otherwise).
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// Parse `[bind:]port` for `-D`. The bind part is optional (default loopback);
/// port 0 means "pick a free port" and is reported with the actual number.
fn parse_bind_port(spec: &str) -> Result<(String, u16), String> {
    let s = spec.trim();
    let (bind, port_str) = match s.rsplit_once(':') {
        Some((b, p)) => (b.trim(), p.trim()),
        None => ("", s),
    };
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port: {port_str}"))?;
    let bind = normalize_bind(bind, DEFAULT_BIND);
    Ok((bind, port))
}

/// Strip IPv6 brackets and default empty binds; also maps "localhost" as-is (the
/// resolver handles it). Remote binds may legitimately be "" (server decides).
fn normalize_bind(bind: &str, default: &str) -> String {
    let b = bind.trim();
    let b = b.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(b);
    if b.is_empty() {
        default.to_string()
    } else {
        b.to_string()
    }
}

/// Parse one `-D` / `-L` / `-R` argument value into a [`ForwardSpec`].
/// `kind` is the flag letter ('D', 'L' or 'R', case-insensitive).
pub fn parse_forward(kind: char, spec: &str) -> Result<ForwardSpec, String> {
    match kind.to_ascii_uppercase() {
        'D' => {
            let (bind, port) = parse_bind_port(spec)?;
            Ok(ForwardSpec::Dynamic { bind, port })
        }
        'L' | 'R' => {
            let remote = kind.to_ascii_uppercase() == 'R';
            let parts: Vec<&str> = spec.trim().split(':').collect();
            // RFC-style IPv6 literals in the middle fields are not supported:
            // accept the two standard forms "port:host:rport" and "bind:port:host:rport".
            let (bind, port_s, host, rport_s) = match parts.as_slice() {
                [p, h, rp] => ("", *p, *h, *rp),
                [b, p, h, rp] => (*b, *p, *h, *rp),
                _ => {
                    return Err(format!(
                        "expected [bind:]port:host:rport, got {spec:?}"
                    ))
                }
            };
            let port: u16 = port_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid port: {port_s}"))?;
            let rport: u16 = rport_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid port: {rport_s}"))?;
            let host = host.trim();
            if host.is_empty() {
                return Err("target host is empty".to_string());
            }
            let default = if remote { "" } else { DEFAULT_BIND };
            let bind = normalize_bind(bind, default);
            if remote {
                Ok(ForwardSpec::Remote { bind, port, host: host.to_string(), rport })
            } else {
                Ok(ForwardSpec::Local { bind, port, host: host.to_string(), rport })
            }
        }
        _ => Err(format!("unknown forward kind {kind:?} (use D, L or R)")),
    }
}

/// Build a [`ForwardSpec`] from UI form fields (the tunnel add-row): validation
/// errors are returned as plain messages for inline display.
pub fn spec_from_parts(
    kind: char,
    bind: &str,
    port_s: &str,
    host: &str,
    rport_s: &str,
) -> Result<ForwardSpec, String> {
    match kind.to_ascii_uppercase() {
        'D' => {
            let port: u16 = port_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid port: {port_s}"))?;
            let bind = normalize_bind(bind, DEFAULT_BIND);
            Ok(ForwardSpec::Dynamic { bind, port })
        }
        'L' | 'R' => {
            let remote = kind.to_ascii_uppercase() == 'R';
            let port: u16 = port_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid port: {port_s}"))?;
            let rport: u16 = rport_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid port: {rport_s}"))?;
            let host = host.trim();
            if host.is_empty() {
                return Err("target host is empty".to_string());
            }
            let default = if remote { "" } else { DEFAULT_BIND };
            let bind = normalize_bind(bind, default);
            if remote {
                Ok(ForwardSpec::Remote { bind, port, host: host.to_string(), rport })
            } else {
                Ok(ForwardSpec::Local { bind, port, host: host.to_string(), rport })
            }
        }
        _ => Err(format!("unknown forward kind {kind:?} (use D, L or R)")),
    }
}

/// Parse one `tunnels` entry from servers.json: `D:spec` / `L:spec` / `R:spec`.
/// Examples: `D:1080`, `L:8080:db.internal:5432`, `R:0.0.0.0:80:localhost:3000`.
pub fn parse_tunnel_entry(entry: &str) -> Result<ForwardSpec, String> {
    let s = entry.trim();
    let Some((kind, spec)) = s.split_once(':') else {
        return Err(format!(
            "missing D:/L:/R: prefix: {s:?}"
        ));
    };
    let mut chars = kind.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!("unknown forward kind {kind:?} (use D, L or R)"));
    };
    parse_forward(c, spec)
}

/// Parse a whole profile `tunnels` list, reporting the failing entry.
pub fn parse_tunnel_list(entries: &[String]) -> Result<Vec<ForwardSpec>, String> {
    entries.iter().map(|e| parse_tunnel_entry(e)).collect()
}

/// Per-forward counters. `conns` counts established tunnels; up/down are bytes
/// moved client→target and target→client respectively (accumulated when a
/// transfer finishes).
#[derive(Default)]
pub struct FwdStats {
    pub conns: AtomicU64,
    pub up: AtomicU64,
    pub down: AtomicU64,
}

impl FwdStats {
    fn conn_established(&self) {
        self.conns.fetch_add(1, Ordering::Relaxed);
    }
    fn add_transfer(&self, up: u64, down: u64) {
        self.up.fetch_add(up, Ordering::Relaxed);
        self.down.fetch_add(down, Ordering::Relaxed);
    }
}

/// -R table: remote listening port → (local target host, port, stats) the
/// incoming connection is pumped to. Shared with the client `Handler` (which
/// receives the server's forwarded-tcpip channels). Keyed by port alone — two -R
/// forwards cannot share a port, and the server may normalize the echoed bind.
pub type RemoteTable = Arc<StdMutex<HashMap<u32, (String, u16, Arc<FwdStats>)>>>;

pub fn new_remote_table() -> RemoteTable {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// The authenticated russh session shared by every tunnel task. `Handle` is not
/// `Clone`, so all channel opens serialize on this mutex (each hold is short).
pub type SharedSession = Arc<tokio::sync::Mutex<client::Handle<crate::session::Handler>>>;

/// One live forward.
struct ForwardRuntime {
    id: u64,
    spec: ForwardSpec,
    /// Display line with the actual bound/assigned port.
    desc: String,
    stats: Arc<FwdStats>,
    /// Local listener task (D/L); None for -R (the listener lives on the server).
    task: Option<tokio::task::JoinHandle<()>>,
    /// -R only: the port the server actually bound (differs from spec.port when
    /// 0 was requested and the server assigned one). Key for the remote table.
    remote_bound: Option<u32>,
}

/// Immutable per-forward view for the UI.
#[derive(Clone)]
pub struct FwdSnapshot {
    pub id: u64,
    pub spec: ForwardSpec,
    pub desc: String,
    pub conns: u64,
    pub up: u64,
    pub down: u64,
}

/// Manages the live forwards of one SSH session: initial bring-up, dynamic
/// add/remove (the terminal-tab popup and the tunnels tab share one controller),
/// listing with stats. `list` is sync and cheap (std mutex, no await inside) so
/// the UI can call it every frame; add/remove are async (session round trips).
pub struct TunnelController {
    session: SharedSession,
    remote: RemoteTable,
    entries: Arc<StdMutex<HashMap<u64, ForwardRuntime>>>,
    next_id: AtomicU64,
}

pub type SharedController = Arc<TunnelController>;

impl TunnelController {
    pub fn new(session: SharedSession, remote: RemoteTable) -> Self {
        Self {
            session,
            remote,
            entries: Arc::new(StdMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    /// Bring the initial set up; on any failure everything already started is
    /// aborted and Err returned (a tunnel set never comes up half-way).
    pub async fn start(&self, specs: &[ForwardSpec]) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        for spec in specs {
            match self.add_forward(spec.clone()).await {
                Ok(desc) => lines.push(desc),
                Err(e) => {
                    self.abort_all().await;
                    return Err(e);
                }
            }
        }
        Ok(lines)
    }

    /// Start one more forward on the live session. Returns the display line
    /// (with the actual bound/assigned port).
    pub async fn add_forward(&self, spec: ForwardSpec) -> Result<String, String> {
        if self.session.lock().await.is_closed() {
            return Err("connection closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stats = Arc::new(FwdStats::default());
        let (rt, desc) = match spec.clone() {
            ForwardSpec::Local { bind, port, host, rport } => {
                let listener = TcpListener::bind((bind.as_str(), port))
                    .await
                    .map_err(|e| format!("cannot listen on {bind}:{port}: {e}"))?;
                let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                let task = tokio::spawn(local_accept_loop(
                    listener,
                    self.session.clone(),
                    host.clone(),
                    rport,
                    stats.clone(),
                ));
                let desc =
                    ForwardSpec::Local { bind: bind.clone(), port: actual, host, rport }.describe();
                (
                    ForwardRuntime { id, spec, desc: desc.clone(), stats, task: Some(task), remote_bound: None },
                    desc,
                )
            }
            ForwardSpec::Dynamic { bind, port } => {
                let listener = TcpListener::bind((bind.as_str(), port))
                    .await
                    .map_err(|e| format!("cannot listen on {bind}:{port}: {e}"))?;
                let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                let task = tokio::spawn(dynamic_accept_loop(
                    listener,
                    self.session.clone(),
                    stats.clone(),
                ));
                let desc = ForwardSpec::Dynamic { bind: bind.clone(), port: actual }.describe();
                (
                    ForwardRuntime { id, spec, desc: desc.clone(), stats, task: Some(task), remote_bound: None },
                    desc,
                )
            }
            ForwardSpec::Remote { bind, port, host, rport } => {
                // bind may be "" (server decides, usually loopback) or "0.0.0.0" etc.
                let replied = self
                    .session
                    .lock()
                    .await
                    .tcpip_forward(bind.clone(), u32::from(port))
                    .await
                    .map_err(|e| format!("server refused remote forward {bind}:{port}: {e}"))?;
                // OpenSSH only echoes the port in the reply when 0 was requested
                // (server-assigned); for a specific port the success reply is
                // empty and russh reports 0 — the bind IS the requested port.
                let bound = if replied == 0 { u32::from(port) } else { replied };
                if let Ok(mut t) = self.remote.lock() {
                    t.insert(bound, (host.clone(), rport, stats.clone()));
                }
                let desc = ForwardSpec::Remote {
                    bind: bind.clone(),
                    port: bound as u16,
                    host,
                    rport,
                }
                .describe();
                (ForwardRuntime { id, spec, desc: desc.clone(), stats, task: None, remote_bound: Some(bound) }, desc)
            }
        };
        if let Ok(mut g) = self.entries.lock() {
            g.insert(id, rt);
        }
        Ok(desc)
    }

    /// Stop one live forward: local listeners are aborted (port freed), -R is
    /// cancelled on the server (exact bind string as registered). Already-open
    /// tunnels finish naturally. Returns the removed forward's display line.
    pub async fn remove_forward(&self, id: u64) -> Result<String, String> {
        let rt = self.entries.lock().ok().and_then(|mut g| g.remove(&id));
        let Some(rt) = rt else {
            return Err(format!("no such forward #{id}"));
        };
        if let Some(t) = rt.task {
            t.abort();
        }
        if let ForwardSpec::Remote { bind, port, .. } = &rt.spec {
            let _ = self
                .session
                .lock()
                .await
                .cancel_tcpip_forward(bind.clone(), u32::from(*port))
                .await;
            if let Ok(mut t) = self.remote.lock() {
                t.remove(&u32::from(*port));
            }
        }
        Ok(rt.desc)
    }

    /// Snapshot of all live forwards (id order), for UI rendering.
    pub fn list(&self) -> Vec<FwdSnapshot> {
        let Some(g) = self.entries.lock().ok() else {
            return Vec::new();
        };
        let mut v: Vec<FwdSnapshot> = g
            .values()
            .map(|rt| FwdSnapshot {
                id: rt.id,
                spec: rt.spec.clone(),
                desc: rt.desc.clone(),
                conns: rt.stats.conns.load(Ordering::Relaxed),
                up: rt.stats.up.load(Ordering::Relaxed),
                down: rt.stats.down.load(Ordering::Relaxed),
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// Stop everything (session teardown).
    pub async fn abort_all(&self) {
        let all: Vec<ForwardRuntime> = self
            .entries
            .lock()
            .map(|mut g| g.drain().map(|(_, v)| v).collect())
            .unwrap_or_default();
        for rt in all {
            if let Some(t) = rt.task {
                t.abort();
            }
            if let ForwardSpec::Remote { bind, port, .. } = &rt.spec {
                let _ = self
                    .session
                    .lock()
                    .await
                    .cancel_tcpip_forward(bind.clone(), u32::from(*port))
                    .await;
            }
            if let Some(bound) = rt.remote_bound {
                if let Ok(mut t) = self.remote.lock() {
                    t.remove(&bound);
                }
            }
        }
    }
}

/// `-L` accept loop: each local connection becomes one direct-tcpip channel.
async fn local_accept_loop(
    listener: TcpListener,
    session: SharedSession,
    host: String,
    rport: u16,
    stats: Arc<FwdStats>,
) {
    loop {
        let Ok((mut tcp, peer)) = listener.accept().await else {
            break; // listener gone (aborted) — nothing to serve
        };
        let session = session.clone();
        let host = host.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            let chan = {
                let s = session.lock().await;
                s.channel_open_direct_tcpip(
                    host.clone(),
                    u32::from(rport),
                    peer.ip().to_string(),
                    u32::from(peer.port()),
                )
                .await
            };
            match chan {
                Ok(channel) => {
                    stats.conn_established();
                    let mut stream = channel.into_stream();
                    // (tcp→server, server→tcp) = (up, down)
                    if let Ok((up, down)) =
                        tokio::io::copy_bidirectional(&mut tcp, &mut stream).await
                    {
                        stats.add_transfer(up, down);
                    }
                }
                Err(_) => {} // server refused: drop the local socket
            }
            let _ = tcp.shutdown().await;
        });
    }
}

/// `-D` accept loop: each local connection speaks SOCKS5 (RFC 1928, no-auth,
/// CONNECT only); the target the client names is opened on the SSH server.
async fn dynamic_accept_loop(listener: TcpListener, session: SharedSession, stats: Arc<FwdStats>) {
    loop {
        let Ok((mut tcp, peer)) = listener.accept().await else {
            break;
        };
        let session = session.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            match socks5_handshake(&mut tcp).await {
                Ok(target) => {
                    let chan = {
                        let s = session.lock().await;
                        s.channel_open_direct_tcpip(
                            target.host.clone(),
                            u32::from(target.port),
                            peer.ip().to_string(),
                            u32::from(peer.port()),
                        )
                        .await
                    };
                    match chan {
                        Ok(channel) => {
                            // Success reply: VER=5 REP=0 RSV=0 ATYP=1 BND.ADDR=0.0.0.0 BND.PORT=0
                            if write_socks_reply(&mut tcp, REP_OK).await.is_ok() {
                                stats.conn_established();
                                let mut stream = channel.into_stream();
                                // (client→server, server→client) = (up, down)
                                if let Ok((up, down)) =
                                    tokio::io::copy_bidirectional(&mut tcp, &mut stream).await
                                {
                                    stats.add_transfer(up, down);
                                }
                            }
                        }
                        Err(_) => {
                            // The SSH server refused the target connection.
                            let _ = write_socks_reply(&mut tcp, REP_REFUSED).await;
                        }
                    }
                }
                // 0 = protocol garbage or the method was already rejected
                // (0xFF sent) — replying again would itself be a violation.
                Err(rep) if rep != 0 => {
                    // Handshake failure worth answering (unsupported cmd/method/ATYP).
                    let _ = write_socks_reply(&mut tcp, rep).await;
                }
                Err(_) => {}
            }
            let _ = tcp.shutdown().await;
        });
    }
}

/// SOCKS5 reply codes used here.
const REP_OK: u8 = 0x00;
const REP_GENERAL_FAIL: u8 = 0x01;
const REP_CMD_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;
const REP_REFUSED: u8 = 0x05;

/// RFC 1928 server-side handshake: greeting (no-auth only), then CONNECT.
/// Returns the requested target, or Err(reply code) — 0 means "drop silently"
/// (protocol garbage not worth answering).
async fn socks5_handshake(tcp: &mut tokio::net::TcpStream) -> Result<SocksTarget, u8> {
    use tokio::io::AsyncReadExt;
    // 1. Greeting: VER NMETHODS METHODS...
    let mut head = [0u8; 2];
    tcp.read_exact(&mut head).await.map_err(|_| 0u8)?;
    let (ver, nmethods) = (head[0], head[1]);
    if ver != 5 || nmethods == 0 {
        return Err(0);
    }
    let mut methods = vec![0u8; nmethods as usize];
    tcp.read_exact(&mut methods).await.map_err(|_| 0u8)?;
    if !methods.contains(&0x00) {
        // No acceptable auth method: 0xFF, then drop.
        let _ = tcp.write_all(&[5, 0xFF]).await;
        return Err(0);
    }
    tcp.write_all(&[5, 0x00]).await.map_err(|_| 0u8)?;

    // 2. Request: VER CMD RSV ATYP ADDR PORT
    let mut req = [0u8; 4];
    tcp.read_exact(&mut req).await.map_err(|_| 0u8)?;
    if req[0] != 5 {
        return Err(0);
    }
    if req[1] != 0x01 {
        // Only CONNECT; BIND and UDP ASSOCIATE are out of scope.
        return Err(REP_CMD_UNSUPPORTED);
    }
    let host = match req[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            tcp.read_exact(&mut ip).await.map_err(|_| 0u8)?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            tcp.read_exact(&mut len).await.map_err(|_| 0u8)?;
            let mut name = vec![0u8; len[0] as usize];
            tcp.read_exact(&mut name).await.map_err(|_| 0u8)?;
            String::from_utf8_lossy(&name).into_owned()
        }
        0x04 => {
            let mut ip = [0u8; 16];
            tcp.read_exact(&mut ip).await.map_err(|_| 0u8)?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => return Err(REP_ATYP_UNSUPPORTED),
    };
    if host.is_empty() {
        return Err(REP_GENERAL_FAIL);
    }
    let mut port_b = [0u8; 2];
    tcp.read_exact(&mut port_b).await.map_err(|_| 0u8)?;
    let port = u16::from_be_bytes(port_b);
    Ok(SocksTarget { host, port })
}

struct SocksTarget {
    host: String,
    port: u16,
}

/// Write the fixed 10-byte success/failure reply (ATYP=1, zero BND).
async fn write_socks_reply(tcp: &mut tokio::net::TcpStream, rep: u8) -> std::io::Result<()> {
    tcp.write_all(&[5, rep, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// Pump one server-initiated `-R` connection to its local target. Called from the
/// client `Handler` (inside russh's session task) — the actual connect+copy is
/// spawned so the SSH session loop is never blocked by a slow local target.
pub fn pump_forwarded_channel<S>(
    channel: russh::Channel<S>,
    host: String,
    port: u16,
    stats: Arc<FwdStats>,
) where
    S: From<(russh::ChannelId, ChannelMsg)> + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let Ok(mut tcp) = tokio::net::TcpStream::connect((host.as_str(), port)).await else {
            return; // local target down: dropping the channel closes the remote leg
        };
        stats.conn_established();
        let mut stream = channel.into_stream();
        // (server→local target, local target→server) = (up, down)
        if let Ok((up, down)) = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await {
            stats.add_transfer(up, down);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dynamic_forms() {
        assert_eq!(
            parse_forward('D', "1080").unwrap(),
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 }
        );
        assert_eq!(
            parse_forward('D', "0.0.0.0:1080").unwrap(),
            ForwardSpec::Dynamic { bind: "0.0.0.0".into(), port: 1080 }
        );
        assert_eq!(
            parse_forward('d', "[::1]:1080").unwrap(),
            ForwardSpec::Dynamic { bind: "::1".into(), port: 1080 }
        );
        assert!(parse_forward('D', "no-port").is_err());
        assert!(parse_forward('D', "abc:99999").is_err());
    }

    #[test]
    fn parse_local_and_remote_forms() {
        assert_eq!(
            parse_forward('L', "8080:db.internal:5432").unwrap(),
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 8080, host: "db.internal".into(), rport: 5432 }
        );
        assert_eq!(
            parse_forward('L', "127.0.0.1:8080:db.internal:5432").unwrap(),
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 8080, host: "db.internal".into(), rport: 5432 }
        );
        assert_eq!(
            parse_forward('R', "80:localhost:3000").unwrap(),
            ForwardSpec::Remote { bind: "".into(), port: 80, host: "localhost".into(), rport: 3000 }
        );
        assert_eq!(
            parse_forward('R', "0.0.0.0:80:localhost:3000").unwrap(),
            ForwardSpec::Remote { bind: "0.0.0.0".into(), port: 80, host: "localhost".into(), rport: 3000 }
        );
        // remote bind defaults to "" (server decides), local defaults to loopback
        match parse_forward('R', "8080:h:80").unwrap() {
            ForwardSpec::Remote { bind, .. } => assert_eq!(bind, ""),
            _ => panic!("expected Remote"),
        }
        assert!(parse_forward('L', "8080:").is_err()); // missing rport
        assert!(parse_forward('L', "a:b:c:d:e").is_err()); // too many fields
        assert!(parse_forward('L', "xx:yy:zz").is_err()); // port not a number
        // empty bind field = the default bind (same as omitting it)
        match parse_forward('L', ":8080:host:80").unwrap() {
            ForwardSpec::Local { bind, .. } => assert_eq!(bind, "127.0.0.1"),
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn parse_tunnel_entries() {
        assert_eq!(
            parse_tunnel_entry("D:1080").unwrap(),
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 }
        );
        assert_eq!(
            parse_tunnel_entry("l:8080:host:80").unwrap(),
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 8080, host: "host".into(), rport: 80 }
        );
        assert_eq!(
            parse_tunnel_entry(" R:80:localhost:3000 ").unwrap(),
            ForwardSpec::Remote { bind: "".into(), port: 80, host: "localhost".into(), rport: 3000 }
        );
        assert!(parse_tunnel_entry("1080").is_err()); // no prefix
        assert!(parse_tunnel_entry("X:1080").is_err()); // unknown kind
        assert!(parse_tunnel_entry("DD:1080").is_err()); // multi-char kind
    }

    #[test]
    fn parse_tunnel_list_reports_entry() {
        let ok = parse_tunnel_list(&["D:1080".into(), "L:8080:h:80".into()]).unwrap();
        assert_eq!(ok.len(), 2);
        let err = parse_tunnel_list(&["D:1080".into(), "bad".into()]);
        assert!(err.is_err());
    }

    #[test]
    fn describe_shapes() {
        assert_eq!(
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 }.describe(),
            "SOCKS5 127.0.0.1:1080"
        );
        assert_eq!(
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 8080, host: "db".into(), rport: 5432 }.describe(),
            "127.0.0.1:8080 -> db:5432"
        );
        assert_eq!(
            ForwardSpec::Remote { bind: "".into(), port: 80, host: "web".into(), rport: 3000 }.describe(),
            "remote :80 -> web:3000"
        );
    }

    #[test]
    fn spec_from_parts_validates() {
        assert_eq!(
            spec_from_parts('d', "", "1080", "", "").unwrap(),
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 }
        );
        assert_eq!(
            spec_from_parts('L', "", "15432", " db.internal ", "5432").unwrap(),
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 15432, host: "db.internal".into(), rport: 5432 }
        );
        assert_eq!(
            spec_from_parts('R', "0.0.0.0", "80", "localhost", "3000").unwrap(),
            ForwardSpec::Remote { bind: "0.0.0.0".into(), port: 80, host: "localhost".into(), rport: 3000 }
        );
        assert!(spec_from_parts('L', "", "8080", "  ", "80").is_err()); // empty host
        assert!(spec_from_parts('L', "", "abc", "h", "80").is_err()); // bad port
        assert!(spec_from_parts('R', "", "80", "h", "x").is_err()); // bad rport
        assert!(spec_from_parts('X', "", "80", "h", "80").is_err()); // unknown kind
    }

    #[test]
    fn to_entry_roundtrip() {
        // canonical entries must parse back to the same rule
        let specs = vec![
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 },
            ForwardSpec::Dynamic { bind: "0.0.0.0".into(), port: 1081 },
            ForwardSpec::Local { bind: "127.0.0.1".into(), port: 8080, host: "db".into(), rport: 5432 },
            ForwardSpec::Local { bind: "192.168.1.2".into(), port: 8081, host: "db".into(), rport: 5433 },
            ForwardSpec::Remote { bind: "".into(), port: 80, host: "web".into(), rport: 3000 },
            ForwardSpec::Remote { bind: "0.0.0.0".into(), port: 81, host: "web".into(), rport: 3001 },
        ];
        for s in specs {
            let entry = s.to_entry();
            assert_eq!(parse_tunnel_entry(&entry).unwrap(), s, "entry: {entry}");
        }
        assert_eq!(
            ForwardSpec::Dynamic { bind: "127.0.0.1".into(), port: 1080 }.to_entry(),
            "D:1080"
        );
    }
}
