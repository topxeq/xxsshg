//! SFTP file operations for the GUI file browser.
//! Engine helpers over `russh_sftp::client::SftpSession` (logic mirrors
//! xxssh 0.5.2 src/sftp.rs, trimmed to what the GUI needs).

use russh_sftp::protocol::OpenFlags;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const BUF: usize = 64 * 1024;

/// Answer codes sent by the UI for a conflict question
pub const ANSWER_OVERWRITE: u8 = 1;
pub const ANSWER_SKIP: u8 = 2;
pub const ANSWER_OVERWRITE_ALL: u8 = 3;
pub const ANSWER_SKIP_ALL: u8 = 4;

/// Conflict policy for transfers (mirrors xxssh TUI's Overwrite enum):
/// ask the UI on each conflict; sticky answers from 覆盖全部/跳过全部 are
/// honored for the rest of the transfer. One policy per transfer operation.
pub struct ConflictPolicy {
    /// 0 = ask each, 1 = overwrite-all, 2 = skip-all
    sticky: AtomicU8,
    ask: mpsc::UnboundedSender<(String, oneshot::Sender<u8>)>,
}

impl ConflictPolicy {
    pub fn new(ask: mpsc::UnboundedSender<(String, oneshot::Sender<u8>)>) -> Self {
        Self { sticky: AtomicU8::new(0), ask }
    }

    /// policy that overwrites everything without asking (temp preview files)
    pub fn overwrite_all() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self { sticky: AtomicU8::new(1), ask: tx }
    }

    /// true = transfer over `dest`, false = skip this item
    pub async fn decide(&self, dest: &str) -> bool {
        match self.sticky.load(Ordering::Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        let (tx, rx) = oneshot::channel();
        if self.ask.send((dest.to_string(), tx)).is_err() {
            return true; // UI gone: keep the old overwrite behavior
        }
        match rx.await {
            Ok(ANSWER_OVERWRITE_ALL) => {
                self.sticky.store(1, Ordering::Relaxed);
                true
            }
            Ok(ANSWER_SKIP_ALL) => {
                self.sticky.store(2, Ordering::Relaxed);
                false
            }
            Ok(ANSWER_OVERWRITE) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    #[allow(dead_code)] // shown in a future detailed-list view
    pub mtime: u32,
}

pub fn join_remote(dir: &str, name: &str) -> String {
    let mut d = dir.trim_end_matches('/');
    if d.is_empty() {
        d = "";
    }
    format!("{}/{}", d, name)
}

pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| match (b.is_dir, a.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

pub async fn list_dir(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> Result<Vec<FileEntry>, String> {
    let read = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    let mut out: Vec<FileEntry> = read
        .map(|e| {
            let md = e.metadata();
            FileEntry {
                name: e.file_name(),
                is_dir: md.is_dir(),
                size: md.size.unwrap_or(0),
                mtime: md.mtime.unwrap_or(0),
            }
        })
        .filter(|e| e.name != "." && e.name != "..")
        .collect();
    sort_entries(&mut out);
    Ok(out)
}

#[allow(dead_code)] // reserved: open-in-app feature
pub async fn remote_is_dir(sftp: &russh_sftp::client::SftpSession, path: &str) -> Option<bool> {
    let md = sftp.metadata(path).await.ok()?;
    Some(md.is_dir())
}

pub async fn mkdir_all(sftp: &russh_sftp::client::SftpSession, path: &str) -> Result<(), String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        return Ok(());
    }
    if sftp.metadata(path).await.is_ok() {
        return Ok(());
    }
    if let Some(idx) = path.rfind('/') {
        let parent = &path[..idx];
        if !parent.is_empty() {
            Box::pin(mkdir_all(sftp, parent)).await?;
        }
    }
    sftp.create_dir(path).await.map_err(|e| e.to_string())
}

pub async fn delete_recursive(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> Result<(), String> {
    if let Ok(entries) = sftp.read_dir(path).await {
        for e in entries {
            let child = join_remote(path, &e.file_name());
            if e.metadata().is_dir() {
                Box::pin(delete_recursive(sftp, &child)).await?;
            } else {
                sftp.remove_file(&child).await.map_err(|er| er.to_string())?;
            }
        }
    }
    match sftp.remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // non-dir paths fail rmdir; fall back to file removal
            sftp.remove_file(path).await.map_err(|_| e.to_string())
        }
    }
}

/// Download a single remote file with progress (done, total).
/// If the local destination exists, `policy` decides overwrite vs skip.
pub async fn download_file(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    local: &Path,
    total: u64,
    progress: &(dyn Fn(u64) + Send + Sync),
    policy: &ConflictPolicy,
) -> Result<u64, String> {
    if local.exists() && !policy.decide(&local.to_string_lossy()).await {
        return Ok(0); // skipped
    }
    let mut from = sftp.open(remote).await.map_err(|e| e.to_string())?;
    let mut to = std::fs::File::create(local).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; BUF];
    let mut done = 0u64;
    loop {
        let n = from.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut to, &buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done.min(total));
    }
    let _ = to.sync_all();
    Ok(done)
}

/// Upload a single local file with progress (done, total).
/// If the remote destination exists, `policy` decides overwrite vs skip.
pub async fn upload_file(
    sftp: &russh_sftp::client::SftpSession,
    local: &Path,
    remote: &str,
    total: u64,
    progress: &(dyn Fn(u64) + Send + Sync),
    policy: &ConflictPolicy,
) -> Result<u64, String> {
    if sftp.metadata(remote).await.is_ok() && !policy.decide(remote).await {
        return Ok(0); // skipped
    }
    let mut from = std::fs::File::open(local).map_err(|e| e.to_string())?;
    let mut to = sftp
        .open_with_flags(remote, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE)
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; BUF];
    let mut done = 0u64;
    loop {
        let n = std::io::Read::read(&mut from, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done.min(total));
    }
    let _ = to.sync_all().await;
    Ok(done)
}

pub fn count_local_bytes(dir: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                let (f, b) = count_local_bytes(&e.path());
                files += f;
                bytes += b;
            } else {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

pub async fn upload_dir_recursive(
    sftp: &russh_sftp::client::SftpSession,
    local_dir: &Path,
    remote_dir: &str,
    progress: &(dyn Fn(u64) + Send + Sync),
    policy: &ConflictPolicy,
) -> Result<u64, String> {
    let mut transferred = 0u64;
    mkdir_all(sftp, remote_dir).await?;
    let rd = std::fs::read_dir(local_dir).map_err(|e| e.to_string())?;
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let rpath = join_remote(remote_dir, &name);
        if ft.is_dir() {
            transferred +=
                Box::pin(upload_dir_recursive(sftp, &e.path(), &rpath, progress, policy)).await?;
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            // upload_file consults the conflict policy when the dest exists
            let done = upload_file(sftp, &e.path(), &rpath, size, progress, policy).await?;
            transferred += done;
            progress(transferred);
        }
    }
    Ok(transferred)
}

pub async fn download_dir_recursive(
    sftp: &russh_sftp::client::SftpSession,
    remote_dir: &str,
    local_dir: &Path,
    progress: &(dyn Fn(u64) + Send + Sync),
    policy: &ConflictPolicy,
) -> Result<u64, String> {
    let mut transferred = 0u64;
    std::fs::create_dir_all(local_dir).map_err(|e| e.to_string())?;
    let entries = list_dir(sftp, remote_dir).await?;
    for e in entries {
        let rpath = join_remote(remote_dir, &e.name);
        let lpath = local_dir.join(&e.name);
        if e.is_dir {
            transferred +=
                Box::pin(download_dir_recursive(sftp, &rpath, &lpath, progress, policy)).await?;
        } else {
            // download_file consults the conflict policy when the dest exists
            let done = download_file(sftp, &rpath, &lpath, e.size, progress, policy).await?;
            transferred += done;
            progress(transferred);
        }
    }
    Ok(transferred)
}

pub async fn count_remote_bytes(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = list_dir(sftp, path).await {
        for e in entries {
            let child = join_remote(path, &e.name);
            if e.is_dir {
                let (f, b) = Box::pin(count_remote_bytes(sftp, &child)).await;
                files += f;
                bytes += b;
            } else {
                files += 1;
                bytes += e.size;
            }
        }
    }
    (files, bytes)
}

/// Arc<Mutex<SftpSession>> convenience wrapper used by the SFTP tab
#[allow(dead_code)] // used by the SFTP tab through list_dir paths
pub async fn list_dir_shared(
    sftp: &Arc<tokio::sync::Mutex<russh_sftp::client::SftpSession>>,
    path: &str,
) -> Result<Vec<FileEntry>, String> {
    let g = sftp.lock().await;
    list_dir(&g, path).await
}

/// Human-readable size (used by the SFTP browser UI)
pub fn fmt_size_pub(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1} GB", n as f32 / (1 << 30) as f32)
    } else if n >= 1 << 20 {
        format!("{:.1} MB", n as f32 / (1 << 20) as f32)
    } else if n >= 1 << 10 {
        format!("{:.1} KB", n as f32 / (1 << 10) as f32)
    } else {
        format!("{n} B")
    }
}

/// UTC "YYYY-MM-DD HH:MM:SS" from unix seconds (no chrono dependency).
pub fn fmt_time_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch to (year, month, day); Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_time_known_values() {
        assert_eq!(fmt_time_unix(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_time_unix(1_000_000_000), "2001-09-09 01:46:40 UTC");
        // leap year day: 2024-02-29 00:00:00 = 1709164800
        assert_eq!(fmt_time_unix(1_709_164_800), "2024-02-29 00:00:00 UTC");
    }
}
