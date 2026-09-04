//! Self-update for xxsshg (`--update` flag and the ☰ "Check for updates" dialog).
//!
//! Protocol mirrors xxssh 0.5.2 src/update.rs (same product card): query
//! `https://magicdo.top/api/products?id=xxssh`, pick the highest isLatest
//! version's entry for THIS platform ("Windows GUI" — must not match the TUI's
//! plain "Windows" entry), download to `<exe>.new`, verify sha256, then
//! `<exe>` → `<exe>.old`, `<exe>.new` → `<exe>`, detached cleanup of `.old`.
//!
//! Two entry points:
//! - `run_cli`  — console flow for `--update` (println progress; GUI-subsystem
//!   builds run it silently, so the ☰ dialog is the primary path)
//! - `check` + `download_and_install` — building blocks for the GUI dialog

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

/// API base for magicdo.top.
const BASE: &str = "https://magicdo.top";
/// Product id on magicdo.top (xxsshg ships on the xxssh card).
const PRODUCT_ID: &str = "xxssh";

/// A version entry from the products API.
#[derive(Debug, Clone, Deserialize)]
struct VersionEntry {
    platform: String,
    version: String,
    file: String,
    #[serde(default)]
    sha256: String,
    #[serde(default, rename = "isLatest")]
    is_latest: bool,
    #[serde(default)]
    arch: String,
}

/// The top-level product object returned by `GET /api/products?id=xxssh`.
#[derive(Debug, Deserialize)]
struct Product {
    #[serde(default)]
    versions: Vec<VersionEntry>,
}

/// Result of `check`: either already current, or an update waiting
#[derive(Debug)]
pub enum UpdateCheck {
    /// `version` = the running version, `latest` = the store's GUI latest
    UpToDate { version: String, latest: String },
    Available { latest: String, url: String, sha256: String },
}

/// Check the store for a newer version than the running binary.
pub fn check() -> Result<UpdateCheck, String> {
    let current = env!("CARGO_PKG_VERSION");
    let product = fetch_product()?;
    // Consider only entries for OUR platform: the xxssh card also hosts the
    // TUI builds, whose version numbers move independently of the GUI build.
    let mine: Vec<VersionEntry> = product
        .versions
        .iter()
        .filter(|v| is_gui_platform(&v.platform))
        .cloned()
        .collect();
    let latest = latest_version(&mine).ok_or("no Windows GUI entry on the server")?;
    if compare_version(&latest, current) <= 0 {
        return Ok(UpdateCheck::UpToDate {
            version: current.to_string(),
            latest,
        });
    }
    let target = mine
        .iter()
        .find(|v| v.version == latest && is_x64_arch(&v.arch))
        .ok_or_else(|| format!("no x64 Windows GUI entry at v{latest}"))?;
    Ok(UpdateCheck::Available {
        latest,
        url: target.file.clone(),
        sha256: target.sha256.clone(),
    })
}

/// the GUI build ships as platform "Windows GUI" on the shared xxssh card —
/// the plain "Windows" entry is the TUI and must never match
fn is_gui_platform(platform: &str) -> bool {
    let p = platform.to_lowercase();
    p.contains("windows") && p.contains("gui")
}

fn is_x64_arch(arch: &str) -> bool {
    let a = arch.to_lowercase();
    a.contains("x64") || a.contains("amd64") || a.contains("universal") || a.is_empty()
}

/// Download `url` (from `check`), verify `sha256`, replace the running exe.
/// `progress` receives (downloaded_bytes, total_bytes).
pub fn download_and_install(
    url: &str,
    sha256: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), String> {
    let exe_path =
        current_exe().map_err(|e| format!("cannot determine exe path: {e}"))?;
    let new_path = exe_for_suffix(&exe_path, "new");

    // the store link can be slow or stall — retry before giving up, and never
    // leave a partial .new behind (a 0-byte leftover is how a failed attempt
    // was first diagnosed)
    let mut last_err = String::new();
    let mut downloaded = false;
    for attempt in 1..=3 {
        match download(url, &new_path, progress) {
            Ok(()) => {
                downloaded = true;
                break;
            }
            Err(e) => {
                last_err = format!("attempt {attempt}/3: {e}");
                let _ = fs::remove_file(&new_path);
                std::thread::sleep(Duration::from_millis(1500));
            }
        }
    }
    if !downloaded {
        return Err(format!("download failed ({last_err})"));
    }

    if !sha256.is_empty() {
        let actual = sha256_file(&new_path)?;
        if !actual.eq_ignore_ascii_case(sha256) {
            let _ = fs::remove_file(&new_path);
            return Err(format!("sha256 mismatch: expected {sha256}, got {actual}"));
        }
    }

    // Replace: exe -> exe.old, exe.new -> exe (rename is Windows-safe while running)
    let old_path = exe_for_suffix(&exe_path, "old");
    let _ = fs::remove_file(&old_path);
    fs::rename(&exe_path, &old_path)
        .map_err(|e| format!("cannot rename current exe: {e}"))?;
    if let Err(e) = fs::rename(&new_path, &exe_path) {
        // Rollback: restore the original.
        if let Err(rerr) = fs::rename(&old_path, &exe_path) {
            return Err(format!(
                "cannot install new exe: {e}; rollback failed: {rerr}; original kept at {}",
                old_path.display()
            ));
        }
        let _ = fs::remove_file(&new_path);
        return Err(format!("cannot install new exe: {e} (rolled back)"));
    }

    schedule_cleanup(&old_path);
    Ok(())
}

/// Relaunch the (already replaced) executable and exit. Used by the GUI's
/// "restart now" after a successful update.
pub fn restart() {
    if let Ok(exe) = current_exe() {
        let _ = Command::new(&exe).spawn();
    }
    std::process::exit(0);
}

/// Console flow for the `--update` flag (parity with xxssh).
pub fn run_cli(lang: crate::i18n::Language) {
    let current = env!("CARGO_PKG_VERSION");
    println!("🔍 Current version: v{current}");
    match check() {
        Err(e) => {
            eprintln!("❌ Failed to fetch latest version info: {e}");
            print_manual_update(lang);
            std::process::exit(1);
        }
        Ok(UpdateCheck::UpToDate { latest, .. }) => {
            println!("📦 Latest version:  v{latest}");
            println!("✅ Already up to date.");
        }
        Ok(UpdateCheck::Available { latest, url, sha256 }) => {
            println!("📦 Latest version:  v{latest}");
            println!("⬆️  Updating v{current} -> v{latest}...");
            let progress = |done: u64, total: u64| {
                if total > 0 {
                    print!(
                        "\r📥   {:.1} / {:.1} MB ({:.0}%)",
                        done as f64 / 1048576.0,
                        total as f64 / 1048576.0,
                        done as f64 / total as f64 * 100.0
                    );
                } else {
                    print!("\r📥   {:.1} MB", done as f64 / 1048576.0);
                }
                let _ = std::io::stdout().flush();
            };
            if let Err(e) = download_and_install(&url, &sha256, &progress) {
                eprintln!("\r❌ Update failed: {e}");
                print_manual_update(lang);
                std::process::exit(1);
            }
            println!("\r✅ Updated to v{latest}. Restart xxsshg to use it.   ");
        }
    }
}

// ---- shared plumbing (mirrors xxssh src/update.rs) ----

fn fetch_product() -> Result<Product, String> {
    let url = format!("{BASE}/api/products?id={PRODUCT_ID}");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;
    let body: Vec<u8> = {
        let mut reader = resp.into_reader();
        let mut v = Vec::new();
        reader.read_to_end(&mut v).map_err(|e| e.to_string())?;
        v
    };
    serde_json::from_slice::<Product>(&body).map_err(|e| format!("parse: {e}"))
}

/// The highest semver among isLatest versions (None if none).
fn latest_version(versions: &[VersionEntry]) -> Option<String> {
    let mut best: Option<String> = None;
    for v in versions {
        if !v.is_latest {
            continue;
        }
        match &best {
            None => best = Some(v.version.clone()),
            Some(b) if compare_version(&v.version, b) > 0 => best = Some(v.version.clone()),
            _ => {}
        }
    }
    best
}

/// Compare two semver-like version strings. Returns >0 if a>b, <0 if a<b, 0 if equal.
pub fn compare_version(a: &str, b: &str) -> i32 {
    let pa = parse_parts(a);
    let pb = parse_parts(b);
    for i in 0..pa.len().max(pb.len()) {
        let an = pa.get(i).copied().unwrap_or(0);
        let bn = pb.get(i).copied().unwrap_or(0);
        if an != bn {
            return an as i32 - bn as i32;
        }
    }
    0
}

fn parse_parts(v: &str) -> Vec<u32> {
    v.split('.')
        .map(|s| {
            let mut n = 0u32;
            for ch in s.chars() {
                if ch.is_ascii_digit() {
                    n = n * 10 + ch.to_digit(10).unwrap_or(0);
                } else {
                    break;
                }
            }
            n
        })
        .collect()
}

/// Resolve the current executable path (strip the ugly `\\?\` prefix).
fn current_exe() -> Result<PathBuf, std::io::Error> {
    let p = std::env::current_exe()?;
    let cleaned = p.to_string_lossy();
    let stripped = cleaned.strip_prefix(r"\\?\").unwrap_or(&cleaned);
    Ok(PathBuf::from(stripped))
}

/// Build a sibling path of `exe` with a suffix appended, e.g. `<exe>.new`.
fn exe_for_suffix(exe: &PathBuf, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(format!(".{suffix}"));
    exe.with_file_name(name)
}

/// Download `url` (relative to BASE if needed) to `path`, reporting progress.
fn download(
    url: &str,
    path: &PathBuf,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), String> {
    let full = if url.starts_with("http") {
        url.to_string()
    } else {
        format!("{BASE}{url}")
    };
    let resp = ureq::get(&full)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| e.to_string())?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut out = fs::File::create(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    progress(0, total);
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// Compute the sha256 hex digest of a file.
fn sha256_file(path: &PathBuf) -> Result<String, String> {
    use std::io::Read;
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = sha256::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}

// ---- minimal SHA-256 (pure Rust, no dependency; copied from xxssh src/update.rs) ----
mod sha256 {
    pub struct Sha256 {
        h: [u32; 8],
        data: Vec<u8>,
        len: u64,
    }
    impl Sha256 {
        pub fn new() -> Self {
            Self {
                h: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
                ],
                data: Vec::new(),
                len: 0,
            }
        }
        pub fn update(&mut self, data: &[u8]) {
            self.data.extend_from_slice(data);
            self.len += data.len() as u64;
            while self.data.len() >= 64 {
                let block: [u8; 64] = self.data[..64].try_into().unwrap();
                self.process(&block);
                self.data.drain(..64);
            }
        }
        pub fn finalize_hex(mut self) -> String {
            let bit_len = self.len * 8;
            self.data.push(0x80);
            while self.data.len() % 64 != 56 {
                self.data.push(0);
            }
            self.data.extend_from_slice(&bit_len.to_be_bytes());
            while self.data.len() >= 64 {
                let block: [u8; 64] = self.data[..64].try_into().unwrap();
                self.process(&block);
                self.data.drain(..64);
            }
            self.h.iter().map(|w| format!("{w:08x}")).collect()
        }
        fn process(&mut self, block: &[u8; 64]) {
            const K: [u32; 64] = [
                0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
                0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
                0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
                0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
                0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
                0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
                0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
                0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
            ];
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let mut a = self.h[0];
            let mut b = self.h[1];
            let mut c = self.h[2];
            let mut d = self.h[3];
            let mut e = self.h[4];
            let mut f = self.h[5];
            let mut g = self.h[6];
            let mut h = self.h[7];
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let mj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(mj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            self.h[0] = self.h[0].wrapping_add(a);
            self.h[1] = self.h[1].wrapping_add(b);
            self.h[2] = self.h[2].wrapping_add(c);
            self.h[3] = self.h[3].wrapping_add(d);
            self.h[4] = self.h[4].wrapping_add(e);
            self.h[5] = self.h[5].wrapping_add(f);
            self.h[6] = self.h[6].wrapping_add(g);
            self.h[7] = self.h[7].wrapping_add(h);
        }
    }
}

/// Start a detached process that deletes `old_path` after a short delay.
fn schedule_cleanup(old_path: &PathBuf) {
    let p = old_path.to_string_lossy().to_string();
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", &format!("ping 127.0.0.1 -n 3 > nul & del \"{p}\"")]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("sleep 3 && rm -f '{p}'")]);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Manual-update hint for the console flow (the GUI dialog points at the store).
fn print_manual_update(_lang: crate::i18n::Language) {
    eprintln!("   Download the latest Windows GUI build from:");
    eprintln!("     {BASE}  (product: xxssh → Windows GUI)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        // guards the hand-copied inline implementation
        let h = sha256::Sha256::new();
        assert_eq!(
            h.finalize_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut h = sha256::Sha256::new();
        h.update(b"abc");
        assert_eq!(
            h.finalize_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // multi-block path (576 bytes of 9x the 64-char string)
        let mut h = sha256::Sha256::new();
        for _ in 0..9 {
            h.update(b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        }
        assert_eq!(
            h.finalize_hex(),
            "2ee05291d319dcdfcb1c27f0e6bb0c0a95cb09fb2bddbe821c966732684cc724"
        );
    }

    #[test]
    fn compare_version_orders() {
        assert_eq!(compare_version("0.5.0", "0.4.0"), 1);
        assert_eq!(compare_version("0.4.0", "0.4.0"), 0);
        assert_eq!(compare_version("0.4", "0.4.0"), 0);
        assert_eq!(compare_version("0.10.0", "0.9.9"), 1);
        assert_eq!(compare_version("1.0", "0.9.9"), 1);
    }
}
