//! Capture the git short hash at build time for the title bar (best effort).

fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=XXSSHG_BUILD_HASH={hash}");
    // Rebuild when HEAD moves so the build number stays fresh
    println!("cargo:rerun-if-changed=../repo-gui/.git/HEAD");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
