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

impl Settings {
    /// Compile-time default for `static Mutex<Settings>` initialisation.
    pub const DEFAULT: Self = Self {
        hk_translate: HotkeyConfig { modifiers: 0x0003, vk: 0x54 },  // Ctrl+Alt+T
        hk_ocr: HotkeyConfig { modifiers: 0x0003, vk: 0x53 },       // Ctrl+Alt+S
        hk_screenshot: HotkeyConfig { modifiers: 0x0003, vk: 0x44 }, // Ctrl+Alt+D
        hk_layout: HotkeyConfig { modifiers: 0x0003, vk: 0x4C },    // Ctrl+Alt+L
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
        let mut parts = Vec::new();
        if self.modifiers & 0x0002 != 0 { parts.push("Ctrl"); }
        if self.modifiers & 0x0001 != 0 { parts.push("Alt"); }
        if self.modifiers & 0x0004 != 0 { parts.push("Shift"); }
        let key = match self.vk {
            0x41..=0x5A => {
                let c = (self.vk as u8) as char;
                return format!("{}+{c}", parts.join("+"));
            }
            0x70..=0x87 => {
                let n = self.vk - 0x70 + 1;
                return format!("{}+F{n}", parts.join("+"));
            }
            _ => format!("0x{:02X}", self.vk),
        };
        format!("{}+{key}", parts.join("+"))
    }

    /// Конвертирует из формата HOTKEY_CLASS (HOTKEYF_*) в RegisterHotKey (MOD_*)
    pub fn from_hotkey_ctrl(vk: u32, hk_mods: u32) -> Self {
        let mut mods = 0u32;
        if hk_mods & 0x02 != 0 { mods |= 0x0002; } // HOTKEYF_CONTROL -> MOD_CONTROL
        if hk_mods & 0x04 != 0 { mods |= 0x0001; } // HOTKEYF_ALT -> MOD_ALT
        if hk_mods & 0x01 != 0 { mods |= 0x0004; } // HOTKEYF_SHIFT -> MOD_SHIFT
        Self { modifiers: mods, vk }
    }

    /// Конвертирует в формат HOTKEY_CLASS: MAKEWORD(vk, hk_mods)
    pub fn to_hotkey_ctrl_param(&self) -> usize {
        let mut hk_mods = 0u8;
        if self.modifiers & 0x0002 != 0 { hk_mods |= 0x02; } // MOD_CONTROL -> HOTKEYF_CONTROL
        if self.modifiers & 0x0001 != 0 { hk_mods |= 0x04; } // MOD_ALT -> HOTKEYF_ALT
        if self.modifiers & 0x0004 != 0 { hk_mods |= 0x01; } // MOD_SHIFT -> HOTKEYF_SHIFT
        (self.vk as usize) | ((hk_mods as usize) << 8)
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
