//! Build-time metadata: embed the git short hash and (on Windows) the
//! application icon + version info as exe resources.

fn main() {
    // Always re-run so the embedded hash stays fresh after commits
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");

    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=XXSSHG_BUILD_HASH={hash}");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/xxssh-icon.ico");
        res.set("FileDescription", "xxsshg - lightweight GUI SSH client");
        res.set("ProductName", "xxsshg");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to compile Windows resources: {e}");
        }
    }
}
