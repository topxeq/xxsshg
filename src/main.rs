//! xxsshg — lightweight GUI SSH client sharing xxssh's configuration.
//!
//! Entry point: load configs from ~/.xxssh (servers.json / settings.json / gui.json),
//! start a background tokio runtime for SSH sessions, and run the eframe GUI.

mod app;
mod gconfig;
mod i18n;
mod session;
mod term;
mod txdef;
mod xconfig;

use app::XxsshgApp;

fn main() -> eframe::Result {
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size)
            .with_min_inner_size([640.0, 400.0])
            .with_title("xxsshg")
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "xxsshg",
        options,
        Box::new(move |cc| {
            install_fonts(cc);
            Ok(Box::new(XxsshgApp::new(
                rt.handle().clone(),
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

/// Register a CJK fallback font from the system so Chinese/Japanese/Korean text
/// renders without bloating the binary with an embedded font.
fn install_fonts(cc: &eframe::CreationContext<'_>) {
    const CANDIDATES: &[&str] = &[
        // Windows
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        // Linux
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
    ];

    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut defs = egui::FontDefinitions::default();
            // ttc collections: `index` selects the face (0 is right for
            // msyh / wqy / noto).
            defs.font_data.insert(
                "cjk_fallback".into(),
                std::sync::Arc::new(egui::epaint::text::FontData {
                    font: bytes.into(),
                    index: 0,
                    tweak: Default::default(),
                }),
            );
            defs.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk_fallback".into());
            defs.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk_fallback".into());
            cc.egui_ctx.set_fonts(defs);
            break;
        }
    }
}
