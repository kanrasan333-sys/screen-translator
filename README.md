# Screen Translator

**A 4-in-1 Windows background utility that replaces Lightshot, Punto Switcher,
TaskbarX, and QTranslate — in a single 3 MB executable with no installer,
no ads, and no bundled telemetry.**

Lives in the system tray and exposes everything through global hotkeys.

---

## What it replaces

| Replaces | With this feature |
| --- | --- |
| **Lightshot** | Region screenshot to clipboard (`Ctrl+Alt+D`) |
| **QTranslate** / Google Translate popup | Clipboard translation with popup (`Ctrl+Alt+T`) and region OCR + translation (`Ctrl+Alt+S`); auto-detects 18+ source languages, optional DeepSeek backend |
| **Punto Switcher** | Manual and automatic keyboard-layout correction for the last typed word (`привет` ⇄ `ghbdtn`) |
| **TaskbarX** / TaskbarCenter | Dynamically keeps taskbar icons centered |

Everything is configurable through a single settings window and runs from the
system tray with a ~20 MB memory footprint.

---

## Features

- **Clipboard translation** — copies the current selection, translates it,
  and shows a popup next to the cursor. The source language is auto-detected
  from the text (Cyrillic → ru/uk, kana → ja, hangul → ko, Han → zh,
  Arabic / Hebrew / Greek / Thai / Devanagari, plus Latin variants
  de/es/fr/it/pt/pl/tr by distinctive characters); the target is the
  currently selected UI language.
- **Two translation backends** — by default uses the free
  [MyMemory](https://mymemory.translated.net/) API (no key required);
  if a DeepSeek API key is set in settings, the much higher-quality
  `deepseek-chat` model is used instead, with automatic fallback to
  MyMemory on any error.
- **Region OCR + translation** — draw a rectangle with the mouse, the
  captured pixels are recognized via [OCR.space](https://ocr.space/) (primary
  engine) with a fallback to the built-in Windows WinRT OCR, then translated.
- **Region screenshot** — capture a screen area directly to the clipboard
  (Lightshot-style), optionally also saved to a folder of your choice.
- **Punto-style layout correction** — manually rewrite the last typed word
  from the wrong keyboard layout into the right one, or let the automatic
  mode detect and fix gibberish in real time as you type.
- **Taskbar icon centering** — dynamically repositions icons to the middle
  of the taskbar and keeps them centered as icons come and go.
- **System tray** — left-click opens settings, right-click shows a
  "Settings / Exit" menu. The console window is hidden.
- **Multi-language UI** — settings, popup, and tray menu translated into
  13 languages (en, ru, es, fr, de, pt, it, pl, tr, uk, zh, ja, ko).
- **Windows autostart** — optional one-click toggle that writes to
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

---

## Default hotkeys

| Hotkey | Action |
| --- | --- |
| `Ctrl+Alt+T` | Translate the currently selected text |
| `Ctrl+Alt+S` | Select a region → OCR → translate |
| `Ctrl+Alt+D` | Screenshot a region to the clipboard |
| `Ctrl+Alt+L` | Fix the keyboard layout of the last typed word |

All hotkeys are remappable from the settings window (click the tray icon).

---

## Build

Requires Rust **edition 2024** (stable 1.85+) and Windows 10 / 11.

```sh
cargo build --release
```

The binary is produced at `target/release/screen-translator.exe` and is
fully self-contained — just copy it anywhere and run.

### OCR.space API key

The default build uses the public demo key `helloworld`, which has very
low rate limits. Get a free personal key at <https://ocr.space/ocrapi>
and bake it in at build time:

```sh
# PowerShell
$env:OCR_SPACE_API_KEY = "your_key_here"; cargo build --release

# cmd
set OCR_SPACE_API_KEY=your_key_here && cargo build --release

# bash / Git Bash
OCR_SPACE_API_KEY=your_key_here cargo build --release
```

If no key is set and the demo quota is exhausted, OCR automatically falls
back to the built-in Windows engine (requires the corresponding language
packs to be installed in Windows).

---

## Usage

1. Run `screen-translator.exe`. A tray icon appears next to the clock
   (possibly hidden under the "show hidden icons" arrow — drag it out to
   pin it).
2. Select any text in any application and press `Ctrl+Alt+T`. The
   translation pops up next to the cursor.
3. Press `Ctrl+Alt+S`, draw a rectangle on screen with the mouse —
   the recognized and translated text appears in the popup.
4. Press `Ctrl+Alt+D` to capture a region straight into the clipboard,
   ready to paste into any chat or document.
5. Click the tray icon to open settings: remap hotkeys, pick a screenshot
   folder, toggle Punto / taskbar centering / autostart.

---

## Dependencies

- [`windows`](https://crates.io/crates/windows) — Win32 API bindings
- [`arboard`](https://crates.io/crates/arboard) — clipboard access
- [`ureq`](https://crates.io/crates/ureq) — HTTP client
- [`serde`](https://crates.io/crates/serde) + `serde_json` — settings persistence
- [`base64`](https://crates.io/crates/base64) — image encoding for OCR.space
- [`chrono`](https://crates.io/crates/chrono) — screenshot filename timestamps
- [`anyhow`](https://crates.io/crates/anyhow) — error handling

External services used:
- MyMemory Translation API (free, no key required) — default translator
- DeepSeek Chat Completions API (paid, optional) — higher-quality
  alternative; configure the key in the settings window
- OCR.space Parse Image API (free, optional key)

---

## Project layout

```
src/
├── main.rs            # entry point, main message loop, hotkey dispatch
├── settings.rs        # settings model, JSON load/save
├── settings_ui.rs     # custom owner-drawn dark settings window
├── tray.rs            # system tray icon (Shell_NotifyIcon)
├── autostart.rs       # Windows Registry Run key for autostart
├── autotype.rs        # automatic Punto-style layout correction
├── layout.rs          # RU ↔ EN keyboard layout maps for Punto
├── taskbar_center.rs  # taskbar icon centering
├── capture.rs         # rectangular screen-region selector overlay
├── screenshot.rs      # pixel capture and PNG encoding
├── ocr.rs             # OCR.space + WinRT OCR
├── translate.rs       # DeepSeek + MyMemory translation, language detection
├── popup.rs           # translation result popup window
├── i18n.rs            # 13-language UI string table
├── theme.rs           # dark theme color palette
└── utils.rs           # UTF-16, urlencode, Win32 input helpers
```

---

## Settings location

Settings are persisted to
`%APPDATA%\screen-translator\settings.json`.
Delete the file to reset everything to defaults.

---

## License

MIT
