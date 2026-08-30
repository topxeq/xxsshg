//! xxsshg — lightweight GUI SSH client sharing xxssh's configuration.
//!
//! Entry point: load configs from ~/.xxssh (servers.json / settings.json / gui.json),
//! start a background tokio runtime for SSH sessions, and run the eframe GUI.

// GUI app: no console window on Windows in release builds (debug keeps the console
// for env_logger output)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fonts;
mod gconfig;
mod i18n;
mod iso_test_tmp;
mod local;
mod session;
mod term;
mod txdef;
mod xconfig;

use app::XxsshgApp;

fn main() -> eframe::Result {
    // --version for install scripts (matches \d+\.\d+\.\d+)
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("xxsshg v{} ({})", env!("CARGO_PKG_VERSION"), env!("XXSSHG_BUILD_HASH"));
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .init();

    let servers_path = xconfig::default_config_path();
    let settings_path = xconfig::settings_path(&servers_path);
    let gui_path = gconfig::default_gui_config_path();

    // Ensure ~/.xxssh exists (first run) — same layout xxssh creates
    let _ = xconfig::ensure(&servers_path);
    let _ = xconfig::ensure_settings(&settings_path);

    let servers = xconfig::load(&servers_path);
    let settings = xconfig::load_settings(&settings_path);
    let gcfg = gconfig::load(&gui_path);

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
        .with_icon(load_icon());
    if gcfg.window.saved {
        // Restore last-session geometry
        viewport = viewport
            .with_position([gcfg.window.x as f32, gcfg.window.y as f32])
            .with_maximized(gcfg.window.maximized);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    // NB: `rt` must stay alive in this scope for the whole app lifetime — a Runtime
    // dropped (e.g. moved into the creator closure, which eframe discards after the
    // first call) silently shuts down all background session tasks.
    let rt_handle = rt.handle().clone();
    let mono_choice = gcfg.font_family.clone();
    eframe::run_native(
        "xxsshg",
        options,
        Box::new(move |cc| {
            install_fonts(cc, &mono_choice);
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
    )
}

/// Window title with version + build number, e.g. "xxsshg v0.1.0 (7dd959a)"
fn window_title() -> String {
    format!(
        "xxsshg v{} ({})",
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
fn install_fonts(cc: &eframe::CreationContext<'_>, mono_choice: &str) {
    fonts::apply_fonts(&cc.egui_ctx, mono_choice);
}
