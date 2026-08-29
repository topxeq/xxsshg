//! Font installation: platform monospace choice for the terminal + CJK fallback.

use std::sync::Arc;

use egui::epaint::text::FontData;

/// Selectable terminal monospace fonts: (settings label, file path).
/// The first existing entry is the "default" (auto) choice.
pub const MONO_CANDIDATES: &[(&str, &str)] = &[
    ("Cascadia Mono", "C:/Windows/Fonts/CascadiaMono.ttf"),
    ("Cascadia Code", "C:/Windows/Fonts/CascadiaCode.ttf"),
    ("Consolas", "C:/Windows/Fonts/consola.ttf"),
    ("Courier New", "C:/Windows/Fonts/cour.ttf"),
    ("Lucida Console", "C:/Windows/Fonts/lucon.ttf"),
    ("Menlo", "/System/Library/Fonts/Menlo.ttc"),
    ("DejaVu Sans Mono", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
    ("Liberation Mono", "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf"),
    ("Noto Sans Mono", "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf"),
];

/// CJK fallback candidates (first existing wins)
const CJK_CANDIDATES: &[&str] = &[
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
    "C:/Windows/Fonts/Deng.ttf",
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
];

/// Labels of the candidates that exist on this machine
pub fn available_monos() -> Vec<&'static str> {
    MONO_CANDIDATES
        .iter()
        .filter(|(_, path)| std::path::Path::new(path).exists())
        .map(|(label, _)| *label)
        .collect()
}

/// Resolve a settings label (empty = auto: first existing candidate)
fn resolve_mono(choice: &str) -> Option<(&'static str, &'static str)> {
    if choice.is_empty() {
        return MONO_CANDIDATES
            .iter()
            .find(|(_, path)| std::path::Path::new(path).exists())
            .copied();
    }
    MONO_CANDIDATES
        .iter()
        .find(|(label, path)| *label == choice && std::path::Path::new(path).exists())
        .copied()
}

/// (Re)install font families. Called at startup and whenever the terminal font
/// setting changes.
pub fn apply_fonts(ctx: &egui::Context, mono_choice: &str) {
    let mut defs = egui::FontDefinitions::default();

    // Terminal monospace (crisper than egui's built-in unhinted monospace)
    if let Some((_label, path)) = resolve_mono(mono_choice) {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data.insert(
                "term_mono".into(),
                Arc::new(FontData { font: bytes.into(), index: 0, tweak: Default::default() }),
            );
            if let Some(family) = defs.families.get_mut(&egui::FontFamily::Monospace) {
                family.insert(0, "term_mono".into());
            }
        }
    }

    // CJK fallback (after the primary fonts; ttc index 0 is right for msyh/wqy/noto)
    for path in CJK_CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data.insert(
                "cjk_fallback".into(),
                Arc::new(FontData { font: bytes.into(), index: 0, tweak: Default::default() }),
            );
            defs.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk_fallback".into());
            defs.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk_fallback".into());
            break;
        }
    }
    ctx.set_fonts(defs);
}
