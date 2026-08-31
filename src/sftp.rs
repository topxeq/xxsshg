//! SFTP file operations for the GUI file browser.
//! Engine helpers over `russh_sftp::client::SftpSession` (logic mirrors
//! xxssh 0.5.2 src/sftp.rs, trimmed to what the GUI needs).

use russh_sftp::protocol::OpenFlags;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const BUF: usize = 64 * 1024;

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
pub async fn download_file(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    local: &Path,
    total: u64,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<u64, String> {
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
pub async fn upload_file(
    sftp: &russh_sftp::client::SftpSession,
    local: &Path,
    remote: &str,
    total: u64,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<u64, String> {
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
                Box::pin(upload_dir_recursive(sftp, &e.path(), &rpath, progress)).await?;
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            let done = upload_file(sftp, &e.path(), &rpath, size, progress).await?;
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
) -> Result<u64, String> {
    let mut transferred = 0u64;
    std::fs::create_dir_all(local_dir).map_err(|e| e.to_string())?;
    let entries = list_dir(sftp, remote_dir).await?;
    for e in entries {
        let rpath = join_remote(remote_dir, &e.name);
        let lpath = local_dir.join(&e.name);
        if e.is_dir {
            transferred +=
                Box::pin(download_dir_recursive(sftp, &rpath, &lpath, progress)).await?;
        } else {
            let done = download_file(sftp, &rpath, &lpath, e.size, progress).await?;
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
