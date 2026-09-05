//! Font installation: platform monospace choice for the terminal + CJK fallback.

use std::sync::Arc;

use egui::epaint::text::{FontData, FontTweak, HintingTarget, SmoothHinting};

/// Terminal text lives at small sizes on low-dpi screens: egui's default hinting
/// keeps advances unhinted (`preserve_linear_metrics: true`), which leaves
/// vertical stems soft. egui positions glyphs from the shaper's advances (not
/// the hinted outline), so enabling horizontal grid-fitting costs no layout
/// drift — only sharpness.
fn sharp_font_data(bytes: Vec<u8>, sharp: bool, latin: bool) -> FontData {
    // ClearType-era fonts (Consolas et al) deliberately skip horizontal
    // grid-fitting in Smooth mode — their stems stay subpixel by design,
    // which reads as a soft halo under egui's grayscale integer renderer
    // (verified: Smooth{plm:true} == Smooth{plm:false} for Consolas, while
    // Target::Mono produces grid-fit outlines). Terminals want the Mono fit.
    let hinting_target = if sharp && latin {
        HintingTarget::Mono
    } else {
        HintingTarget::Smooth(SmoothHinting {
            light: false,
            symmetric_rendering: true,
            preserve_linear_metrics: !sharp,
        })
    };
    FontData {
        font: bytes.into(),
        index: 0,
        tweak: FontTweak {
            hinting: Some(true),
            hinting_target,
            // sharp: rasterize only at whole-pixel positions. The terminal
            // places every cell on integer pixels, so egui's subpixel binning
            // (1/4px glyph offsets) just adds a gray halo around each glyph.
            subpixel_binning: if sharp { Some(false) } else { None },
            ..Default::default()
        },
    }
}

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

/// Vertical baseline correction for CJK glyphs, as a fraction of font size.
/// Computed in [`apply_fonts`] from the real font metrics (hhea ascent/descent)
/// of the Latin mono font vs the CJK fallback, so it scales with font size.
pub static CJK_BASELINE_SHIFT_EM: std::sync::OnceLock<f32> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn apply_fonts_for_test() {
    let mono = MONO_CANDIDATES
        .iter()
        .find(|(_, p)| std::path::Path::new(p).exists())
        .map(|(_, p)| *p);
    if let Some(cjk) = CJK_CANDIDATES
        .iter()
        .find(|p| std::path::Path::new(p).exists())
    {
        let _ = mono;
        compute_baseline_shift(cjk);
    }
}

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
pub fn apply_fonts(ctx: &egui::Context, mono_choice: &str, sharp: bool) {
    let mut defs = egui::FontDefinitions::default();

    // Terminal monospace (crisper than egui's built-in unhinted monospace)
    if let Some((_label, path)) = resolve_mono(mono_choice) {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data.insert("term_mono".into(), Arc::new(sharp_font_data(bytes, sharp, true)));
            if let Some(family) = defs.families.get_mut(&egui::FontFamily::Monospace) {
                family.insert(0, "term_mono".into());
            }
        }
    }

    // CJK fallback (after the primary fonts; ttc index 0 is right for msyh/wqy/noto)
    for path in CJK_CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            // The CJK font's glyphs sit higher in their em box than the Latin
            // terminal font; nudge them down so mixed lines align. Tunable via
            // XXSSHG_CJK_SHIFT (fraction of font size, default 0.12).
            // smooth hinting even in sharp mode: Mono grid-fitting breaks dense
            // CJK strokes at these sizes (latin=true only for the terminal font)
            defs.font_data.insert(
                "cjk_fallback".into(),
                Arc::new(sharp_font_data(bytes, sharp, false)),
            );
            defs.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk_fallback".into());
            defs.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk_fallback".into());
            compute_baseline_shift(path);
            break;
        }
    }
    ctx.set_fonts(defs);
}

/// epaint positions each glyph on the baseline of its OWN font metrics
/// (row_height = ascent - descent + line_gap). The CJK fallback font has
/// different metrics than the Latin mono font, so per-cell centered drawing
/// puts CJK glyphs at the wrong height. Compute the exact em-fraction shift
/// that aligns the CJK baseline with the Latin baseline.
fn compute_baseline_shift(cjk_path: &str) {
    use skrifa::instance::Size as SkrifaSize;
    let Some((_, latin_path)) = resolve_mono("") else { return };
    let Ok(cjk_bytes) = std::fs::read(cjk_path) else { return };
    let (Ok(cjk_font), Ok(latin_bytes)) = (
        skrifa::FontRef::from_index(&cjk_bytes, 0),
        std::fs::read(latin_path),
    ) else {
        return;
    };
    let Ok(latin_font) = skrifa::FontRef::from_index(&latin_bytes, 0) else { return };
    let em = |f: &skrifa::FontRef| -> (f32, f32, f32) {
        // epaint uses skrifa Metrics (ascent/descent/leading are em fractions
        // when constructed with a nominal size); a size of 1 em is enough here.
        let m = skrifa::metrics::Metrics::new(f, SkrifaSize::new(1.0), skrifa::instance::LocationRef::default());
        (m.ascent, m.descent, m.leading)
    };
    let (asc_l, desc_l, gap_l) = em(&latin_font);
    let (asc_c, desc_c, gap_c) = em(&cjk_font);
    let _ = (desc_l, gap_l, desc_c, gap_c);
    // Paint uses TOP alignment: baseline = draw_y + ascent(font).
    // Shift CJK glyphs by the ascent difference so both baselines coincide.
    let shift_em = asc_l - asc_c;
    let _ = CJK_BASELINE_SHIFT_EM.set(shift_em);
}

#[cfg(test)]
mod shift_tests {
    #[test]
    fn baseline_shift_value() {
        crate::fonts::apply_fonts_for_test();
        let v = crate::fonts::CJK_BASELINE_SHIFT_EM.get();
        println!("CJK_BASELINE_SHIFT_EM = {:?}", v);
        assert!(v.is_some());
        let v = v.unwrap();
        assert!(v.abs() < 1.0, "shift out of sane range: {v}");
    }
}

#[cfg(test)]
mod hinting_probe_tests {
    use super::*;

    /// Collects outline points so different hinting targets can be compared.
    struct PointCollector(Vec<String>);
    impl skrifa::outline::OutlinePen for PointCollector {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("M {x:.2} {y:.2}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("L {x:.2} {y:.2}"));
        }
        fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
            self.0.push(format!("Q {cx:.2} {cy:.2} {x:.2} {y:.2}"));
        }
        fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
            self.0.push(format!("C {c1x:.2} {c1y:.2} {c2x:.2} {c2y:.2} {x:.2} {y:.2}"));
        }
        fn close(&mut self) {
            self.0.push("Z".into());
        }
    }

    fn outline_at(target: skrifa::outline::Target, ch: char) -> Option<Vec<String>> {
        use skrifa::MetadataProvider as _;
        let bytes = std::fs::read("C:/Windows/Fonts/consola.ttf").ok()?;
        let font = skrifa::FontRef::from_index(&bytes, 0).ok()?;
        let glyphs = font.outline_glyphs();
        let gid = font.charmap().map(ch)?;
        let glyph = glyphs.get(gid)?;
        let hinting = skrifa::outline::HintingInstance::new(
            &glyphs,
            skrifa::instance::Size::new(18.0),
            skrifa::instance::LocationRef::default(),
            target,
        )
        .ok()?;
        let mut pen = PointCollector(Vec::new());
        glyph
            .draw(
                skrifa::outline::DrawSettings::hinted(&hinting, false),
                &mut pen,
            )
            .ok()?;
        Some(pen.0)
    }

    /// DECISIVE probe: do the different hinting targets actually produce
    /// different (grid-fit) outlines for Consolas on this machine?
    /// If these are all identical, skrifa is not hinting this font and no
    /// amount of hinting_target configuration can change sharpness.
    #[test]
    fn consolas_hinting_targets_differ() {
        let smooth_default = outline_at(
            skrifa::outline::Target::Smooth {
                mode: skrifa::outline::SmoothMode::Normal,
                symmetric_rendering: true,
                preserve_linear_metrics: true,
            },
            'M',
        )
        .expect("consola.ttf missing or unusable");
        let smooth_gridfit = outline_at(
            skrifa::outline::Target::Smooth {
                mode: skrifa::outline::SmoothMode::Normal,
                symmetric_rendering: true,
                preserve_linear_metrics: false,
            },
            'M',
        )
        .expect("outline failed");
        let mono = outline_at(skrifa::outline::Target::Mono, 'M').expect("outline failed");

        eprintln!("default lines: {}", smooth_default.len());
        eprintln!("gridfit lines: {}", smooth_gridfit.len());
        eprintln!("mono     lines: {}", mono.len());
        eprintln!("default == gridfit: {}", smooth_default == smooth_gridfit);
        eprintln!("gridfit == mono:    {}", smooth_gridfit == mono);
        let sample_d: Vec<_> = smooth_default.iter().take(4).cloned().collect();
        let sample_g: Vec<_> = smooth_gridfit.iter().take(4).cloned().collect();
        let sample_m: Vec<_> = mono.iter().take(4).cloned().collect();
        eprintln!("default sample: {sample_d:?}");
        eprintln!("gridfit sample: {sample_g:?}");
        eprintln!("mono     sample: {sample_m:?}");
    }
}
