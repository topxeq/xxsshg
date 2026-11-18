//! xxsshg — lightweight GUI SSH client sharing xxssh's configuration.
//!
//! Entry point: load configs from ~/.xxssh (servers.json / settings.json / gui.json),
//! start a background tokio runtime for SSH sessions, and run the eframe GUI.

// GUI app: no console window on Windows in release builds (debug keeps the console
// for env_logger output)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod diag;
mod fonts;
mod gconfig;
mod i18n;
mod local;
mod session;
mod sftp;
mod sftpui;
mod term;
mod termup;

// The variants are separate binaries over the same source (see Cargo.toml
// [[bin]]); building with both features is always a mistake.
#[cfg(all(feature = "glow", feature = "wgpu"))]
compile_error!("build ONE variant: default (glow) or --no-default-features --features wgpu");

mod tunnel;
mod txdef;
mod update;
mod xconfig;

use app::XxsshgApp;

fn main() -> eframe::Result {
    // --version for install scripts (matches \d+\.\d+\.\d+)
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "{} v{} ({})",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("XXSSHG_BUILD_HASH")
        );
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .init();

    // Release GUI builds have no console (windows_subsystem="windows"): a
    // startup panic — e.g. eframe/glow failing to create the OpenGL context
    // on a VM without GPU acceleration — used to die with ZERO feedback
    // ("double click, instant exit"). Route panics to a native error box
    // (first occurrence only, no dialog spam from worker threads) and to
    // the session-debug log so the cause is always visible.
    std::panic::set_hook(Box::new(|info| {
        let mut msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "unknown panic".into(),
            },
        };
        if let Some(loc) = info.location() {
            msg = format!("{msg} ({}:{})", loc.file(), loc.line());
        }
        diag::log(&format!("BOOT PANIC: {msg}"));
        if !PANIC_BOX_SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) {
            show_error_box(&format!(
                "xxsshg crashed:\n{msg}\n\n{}\n~/.xxssh/session-debug.log",
                "（详细日志见 ~/.xxssh/session-debug.log / see the log for details）"
            ));
        }
    }));

    // sweep a leftover .old backup from a previous self-update
    update::cleanup_old_at_startup();

    let servers_path = xconfig::default_config_path();
    let settings_path = xconfig::settings_path(&servers_path);
    let gui_path = gconfig::default_gui_config_path();

    // Ensure ~/.xxssh exists (first run) — same layout xxssh creates
    let _ = xconfig::ensure(&servers_path);
    let _ = xconfig::ensure_settings(&settings_path);

    let servers = xconfig::load(&servers_path);
    let settings = xconfig::load_settings(&settings_path);
    let gcfg = gconfig::load(&gui_path);

    // --update: self-update from magicdo.top (console flow, parity with xxssh;
    // GUI-subsystem builds run it without console output — the ☰ menu dialog
    // is the primary path)
    if args.iter().any(|a| a == "--update") {
        update::run_cli(settings.language);
        return Ok(());
    }

    // Background tokio runtime for SSH sessions
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");

    let window_size = [gcfg.window.width as f32, gcfg.window.height as f32];
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(window_size)
        .with_min_inner_size([640.0, 400.0])
        .with_title(window_title())
        .with_icon(load_icon())
        // OS-level file drag&drop: dropping Explorer files uploads them (SFTP tab)
        .with_drag_and_drop(true);
    if gcfg.window.saved {
        // Restore last-session geometry
        viewport = viewport
            .with_position([gcfg.window.x as f32, gcfg.window.y as f32])
            .with_maximized(gcfg.window.maximized);
    }

    // Renderer per build variant. glow (default): raw OpenGL, best on real
    // GPUs. wgpu: D3D12 with an adapter selector that prefers hardware but
    // falls back to the software rasterizer (WARP / "Microsoft Basic Render
    // Driver", DeviceType::Cpu) — the whole point of the wgpu variant is to
    // start on VMs with no GPU driver at all, where glow dies on OpenGL < 2.0.
    #[cfg(feature = "glow")]
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    #[cfg(feature = "wgpu")]
    let options = {
        let selector = |adapters: &[eframe::wgpu::Adapter],
                        _surface: Option<&eframe::wgpu::Surface<'_>>|
         -> Result<eframe::wgpu::Adapter, String> {
            adapters
                .iter()
                .min_by_key(|a| match a.get_info().device_type {
                    // software rasterizer last
                    eframe::wgpu::DeviceType::Cpu | eframe::wgpu::DeviceType::Other => 1,
                    _ => 0,
                })
                .cloned()
                .ok_or_else(|| {
                    "no wgpu adapter available (no GPU driver and no software rasterizer)".into()
                })
        };
        eframe::NativeOptions {
            viewport,
            renderer: eframe::Renderer::Wgpu,
            wgpu_options: eframe::WgpuConfiguration {
                wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(
                    eframe::egui_wgpu::WgpuSetupCreateNew {
                        native_adapter_selector: Some(std::sync::Arc::new(selector)),
                        device_descriptor: std::sync::Arc::new(|_adapter| {
                            eframe::wgpu::DeviceDescriptor {
                                label: Some("xxsshg".into()),
                                ..Default::default()
                            }
                        }),
                        ..eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle()
                    },
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    };

    // NB: `rt` must stay alive in this scope for the whole app lifetime — a Runtime
    // dropped (e.g. moved into the creator closure, which eframe discards after the
    // first call) silently shuts down all background session tasks.
    let rt_handle = rt.handle().clone();
    let mono_choice = gcfg.font_family.clone();
    let sharp_font = gcfg.sharp_font;
    let res = eframe::run_native(
        "xxsshg",
        options,
        Box::new(move |cc| {
            install_fonts(cc, &mono_choice, sharp_font);
            Ok(Box::new(XxsshgApp::new(
                rt_handle.clone(),
                servers_path,
                settings_path,
                gui_path,
                servers,
                settings,
                gcfg,
            )))
        }),
    );
    if let Err(e) = &res {
        // eframe can return instead of panicking (window/GL creation failure);
        // same visibility contract as the panic hook above.
        let msg = format!("eframe: {e}");
        diag::log(&format!("BOOT FAIL: {msg}"));
        show_error_box(&format!(
            "xxsshg failed to start:\n{msg}\n\n提示/hint: OpenGL 相关报错通常是显卡驱动过旧（虚拟机请安装 VMware Tools 并开启 3D 加速 / install guest tools & enable 3D acceleration）\n~/.xxssh/session-debug.log"
        ));
    }
    res
}

/// One-shot flag: the panic hook shows at most one error box per process
/// (tokio worker panics otherwise could spam dialogs).
static PANIC_BOX_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Native error box (MB_ICONERROR). GUI-subsystem builds have no console and
/// no new deps are wanted — declare user32's MessageBoxW directly (user32 is
/// already in the import table).
#[cfg(windows)]
fn show_error_box(text: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, utype: u32) -> i32;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let mut cap: Vec<u16> = "xxsshg".encode_utf16().collect();
    cap.push(0);
    unsafe { MessageBoxW(0, wide.as_ptr(), cap.as_ptr(), 0x10) };
}

#[cfg(not(windows))]
fn show_error_box(_text: &str) {}

/// Window title with binary name (xxsshg / xxsshg-wgpu) + version + build hash
fn window_title() -> String {
    format!(
        "{} v{} ({})",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("XXSSHG_BUILD_HASH")
    )
}

/// Embed the xxssh icon as the window icon (best effort)
fn load_icon() -> egui::IconData {
    const ICON_PNG: &[u8] = include_bytes!("../assets/xxssh-icon.png");
    let img = image::load_from_memory(ICON_PNG).expect("embedded icon must be valid PNG");
    let rgba = img.to_rgba8();
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

/// Install fonts (see fonts::apply_fonts)
fn install_fonts(cc: &eframe::CreationContext<'_>, mono_choice: &str, sharp: bool) {
    fonts::apply_fonts(&cc.egui_ctx, mono_choice, sharp);
}
