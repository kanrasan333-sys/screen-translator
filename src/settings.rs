use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HotkeyConfig {
    pub modifiers: u32, // MOD_CONTROL=2, MOD_ALT=1, MOD_SHIFT=4
    pub vk: u32,        // virtual key code
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub hk_translate: HotkeyConfig,
    pub hk_ocr: HotkeyConfig,
    pub hk_screenshot: HotkeyConfig,
    pub hk_layout: HotkeyConfig,
    /// "Open CMD in active Explorer folder" hotkey.
    #[serde(default = "default_hk_explorer_cmd")]
    pub hk_explorer_cmd: HotkeyConfig,
    pub screenshot_folder: String,
    #[serde(default = "yes")]
    pub punto_enabled: bool,
    #[serde(default = "yes")]
    pub taskbar_center_enabled: bool,
    /// ISO 639-1 language code for the UI ("en", "ru", "es", ...).
    #[serde(default = "default_lang")]
    pub language: String,
}

fn yes() -> bool { true }
fn default_lang() -> String { "en".to_string() }
fn default_hk_explorer_cmd() -> HotkeyConfig {
    HotkeyConfig { modifiers: 0x0003, vk: 0x4B } // Ctrl+Alt+K
}

impl Settings {
    /// Compile-time default for `static Mutex<Settings>` initialisation.
    pub const DEFAULT: Self = Self {
        hk_translate: HotkeyConfig { modifiers: 0x0003, vk: 0x54 },  // Ctrl+Alt+T
        hk_ocr: HotkeyConfig { modifiers: 0x0003, vk: 0x53 },       // Ctrl+Alt+S
        hk_screenshot: HotkeyConfig { modifiers: 0x0003, vk: 0x44 }, // Ctrl+Alt+D
        hk_layout: HotkeyConfig { modifiers: 0x0003, vk: 0x4C },    // Ctrl+Alt+L
        hk_explorer_cmd: HotkeyConfig { modifiers: 0x0003, vk: 0x4B }, // Ctrl+Alt+K
        screenshot_folder: String::new(),
        punto_enabled: true,
        taskbar_center_enabled: true,
        language: String::new(), // filled with "en" on load if missing
    };
}

impl Default for Settings {
    fn default() -> Self {
        let mut s = Self::DEFAULT;
        s.language = default_lang();
        s
    }
}

impl HotkeyConfig {
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.modifiers & 0x0002 != 0 { parts.push("Ctrl".into()); }
        if self.modifiers & 0x0001 != 0 { parts.push("Alt".into()); }
        if self.modifiers & 0x0004 != 0 { parts.push("Shift".into()); }
        let key = match self.vk {
            0x41..=0x5A => ((self.vk as u8) as char).to_string(),
            0x70..=0x87 => format!("F{}", self.vk - 0x70 + 1),
            0x08 => "Backspace".into(),
            0x09 => "Tab".into(),
            0x0D => "Enter".into(),
            0x1B => "Esc".into(),
            0x20 => "Space".into(),
            0x21 => "PgUp".into(),
            0x22 => "PgDn".into(),
            0x23 => "End".into(),
            0x24 => "Home".into(),
            0x25 => "Left".into(),
            0x26 => "Up".into(),
            0x27 => "Right".into(),
            0x28 => "Down".into(),
            0x2D => "Ins".into(),
            0x2E => "Del".into(),
            0x30..=0x39 => ((self.vk as u8) as char).to_string(),
            0x6F => "/".into(),
            0xBA => ";".into(),
            0xBB => "=".into(),
            0xBC => ",".into(),
            0xBD => "-".into(),
            0xBE => ".".into(),
            0xBF => "/".into(),
            0xC0 => "`".into(),
            0xDB => "[".into(),
            0xDC => "\\".into(),
            0xDD => "]".into(),
            0xDE => "'".into(),
            _ => format!("0x{:02X}", self.vk),
        };
        parts.push(key);
        parts.join("+")
    }

}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    let dir = PathBuf::from(appdata).join("screen-translator");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.json")
}

pub fn load() -> Settings {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => {
            let s = Settings::default();
            save(&s);
            s
        }
    }
}

pub fn save(settings: &Settings) {
    let path = settings_path();
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}
