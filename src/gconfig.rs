//! GUI-specific settings: `~/.xxssh/gui.json` (env `XXSSH_GUI_CONFIG` overrides).
//!
//! This file is owned by xxsshg only; xxssh does not read it. All fields have
//! defaults so a missing/corrupt file falls back gracefully.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bell handling on terminal BEL (0x07)
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BellMode {
    /// Ignore BEL entirely (xxssh's default)
    #[default]
    Mute,
    /// Flash the window/tab title area
    Flash,
    /// Play the system beep via stdout BEL (works where the terminal allows it)
    Sound,
}

/// Window size/position
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub maximized: bool,
    /// Restored-on-start position (outer rect, monitor space); valid when `saved`
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    /// Whether a previous session recorded its geometry
    #[serde(default)]
    pub saved: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self { width: 1100, height: 720, maximized: false, x: 0, y: 0, saved: false }
    }
}

/// Theme
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// GUI settings (gui.json)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GuiConfig {
    /// Schema version for future migrations
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub window: WindowState,
    #[serde(default)]
    pub theme: Theme,
    /// Base UI / terminal font size in points
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Font family override; empty = built-in monospace + system CJK fallback
    #[serde(default)]
    pub font_family: String,
    /// Terminal scrollback buffer size in lines
    #[serde(default = "default_scrollback")]
    pub scrollback_lines: u32,
    /// Copy to clipboard on text selection (xterm style)
    #[serde(default = "default_true")]
    pub copy_on_select: bool,
    /// Confirm before quitting while a session is open
    #[serde(default = "default_true")]
    pub confirm_on_quit: bool,
    #[serde(default)]
    pub show_sidebar: bool,
    #[serde(default)]
    pub bell: BellMode,
    /// Invert mouse wheel direction in the terminal (default: traditional,
    /// wheel up scrolls back into history)
    #[serde(default)]
    pub invert_scrolling: bool,
    /// Hotkey to close the active tab (format "Ctrl+W"); empty = disabled
    #[serde(default = "default_hk_close")]
    pub hotkey_close_tab: String,
    /// Hotkey to open a new local CMD tab (format "Ctrl+N"); empty = disabled
    #[serde(default = "default_hk_new_cmd")]
    pub hotkey_new_cmd: String,
    /// Last Quick Connect values (password is never persisted)
    #[serde(default)]
    pub quick_host: String,
    #[serde(default = "default_port_qc")]
    pub quick_port: u16,
    #[serde(default)]
    pub quick_user: String,
}

fn default_port_qc() -> u16 {
    22
}
fn default_hk_close() -> String {
    "Ctrl+W".into()
}
fn default_hk_new_cmd() -> String {
    "Ctrl+N".into()
}

/// Parse a hotkey string like "Ctrl+Shift+W" into (ctrl, shift, alt, egui key).
/// Returns None when unparseable (the hotkey is then disabled).
pub fn parse_hotkey(s: &str) -> Option<(bool, bool, bool, egui::Key)> {
    use egui::Key;
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key: Option<Key> = None;
    for part in s.split('+') {
        match part.trim().to_lowercase().as_str() {
            "" => {}
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "opt" | "option" => alt = true,
            "a" => key = Some(Key::A),
            "b" => key = Some(Key::B),
            "c" => key = Some(Key::C),
            "d" => key = Some(Key::D),
            "e" => key = Some(Key::E),
            "f" => key = Some(Key::F),
            "g" => key = Some(Key::G),
            "h" => key = Some(Key::H),
            "i" => key = Some(Key::I),
            "j" => key = Some(Key::J),
            "k" => key = Some(Key::K),
            "l" => key = Some(Key::L),
            "m" => key = Some(Key::M),
            "n" => key = Some(Key::N),
            "o" => key = Some(Key::O),
            "p" => key = Some(Key::P),
            "q" => key = Some(Key::Q),
            "r" => key = Some(Key::R),
            "s" => key = Some(Key::S),
            "t" => key = Some(Key::T),
            "u" => key = Some(Key::U),
            "v" => key = Some(Key::V),
            "w" => key = Some(Key::W),
            "x" => key = Some(Key::X),
            "y" => key = Some(Key::Y),
            "z" => key = Some(Key::Z),
            "0" => key = Some(Key::Num0),
            "1" => key = Some(Key::Num1),
            "2" => key = Some(Key::Num2),
            "3" => key = Some(Key::Num3),
            "4" => key = Some(Key::Num4),
            "5" => key = Some(Key::Num5),
            "6" => key = Some(Key::Num6),
            "7" => key = Some(Key::Num7),
            "8" => key = Some(Key::Num8),
            "9" => key = Some(Key::Num9),
            "f1" => key = Some(Key::F1),
            "f2" => key = Some(Key::F2),
            "f3" => key = Some(Key::F3),
            "f4" => key = Some(Key::F4),
            "f5" => key = Some(Key::F5),
            "f6" => key = Some(Key::F6),
            "f7" => key = Some(Key::F7),
            "f8" => key = Some(Key::F8),
            "f9" => key = Some(Key::F9),
            "f10" => key = Some(Key::F10),
            "f11" => key = Some(Key::F11),
            "f12" => key = Some(Key::F12),
            "enter" | "return" => key = Some(Key::Enter),
            "esc" | "escape" => key = Some(Key::Escape),
            "tab" => key = Some(Key::Tab),
            "space" => key = Some(Key::Space),
            "home" => key = Some(Key::Home),
            "end" => key = Some(Key::End),
            "pageup" => key = Some(Key::PageUp),
            "pagedown" => key = Some(Key::PageDown),
            "+" | "plus" => key = Some(Key::Plus),
            "-" | "minus" => key = Some(Key::Minus),
            "=" => key = Some(Key::Equals),
            _ => return None,
        }
    }
    key.map(|k| (ctrl, shift, alt, k))
}

fn default_version() -> u32 {
    1
}
fn default_font_size() -> f32 {
    14.0
}
fn default_scrollback() -> u32 {
    10000
}
fn default_true() -> bool {
    true
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            window: WindowState::default(),
            theme: Theme::default(),
            font_size: default_font_size(),
            font_family: String::new(),
            scrollback_lines: default_scrollback(),
            copy_on_select: true,
            confirm_on_quit: true,
            show_sidebar: true,
            bell: BellMode::default(),
            invert_scrolling: false,
            hotkey_close_tab: default_hk_close(),
            hotkey_new_cmd: default_hk_new_cmd(),
            quick_host: String::new(),
            quick_port: 22,
            quick_user: String::from("root"),
        }
    }
}

/// gui.json path: `XXSSH_GUI_CONFIG` env var takes priority; else `~/.xxssh/gui.json`
pub fn default_gui_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("XXSSH_GUI_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xxssh")
        .join("gui.json")
}

/// Load gui.json; missing/corrupt file -> defaults (never fails)
pub fn load(path: &PathBuf) -> GuiConfig {
    let Ok(text) = fs::read_to_string(path) else {
        return GuiConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Save gui.json (auto-create parent dir; Unix 0600 — it contains no secrets but
/// stays consistent with the other files in ~/.xxssh)
pub fn save(path: &PathBuf, cfg: &GuiConfig) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(io::Error::other)?;
    fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("xxsshg_gui_{}_{}", name, std::process::id()));
        p
    }

    #[test]
    fn missing_file_returns_defaults() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        let cfg = load(&path);
        assert_eq!(cfg.font_size, 14.0);
        assert_eq!(cfg.scrollback_lines, 10000);
        assert!(cfg.copy_on_select);
        assert_eq!(cfg.theme, Theme::Dark);
        assert_eq!(cfg.bell, BellMode::Mute);
    }

    #[test]
    fn corrupt_file_returns_defaults() {
        let path = temp_path("corrupt");
        fs::write(&path, "not json {").unwrap();
        assert_eq!(load(&path), GuiConfig::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let path = temp_path("partial");
        fs::write(&path, r#"{"font_size": 18}"#).unwrap();
        let cfg = load(&path);
        assert_eq!(cfg.font_size, 18.0);
        // Everything else defaulted
        assert_eq!(cfg.scrollback_lines, 10000);
        assert_eq!(cfg.version, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_load_roundtrip() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let cfg = GuiConfig {
            font_size: 16.0,
            theme: Theme::Light,
            bell: BellMode::Flash,
            show_sidebar: false,
            ..Default::default()
        };
        save(&path, &cfg).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.font_size, 16.0);
        assert_eq!(loaded.theme, Theme::Light);
        assert_eq!(loaded.bell, BellMode::Flash);
        assert!(!loaded.show_sidebar);
        let _ = fs::remove_file(&path);
    }
}
