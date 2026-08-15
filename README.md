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
| **Punto Switcher** | Manual and automatic keyboard-layout correction across English, Russian and Ukrainian (`привет` ⇄ `ghbdtn`, `привіт` ⇄ `ghbdsn`) |
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
- **Draw on the capture before you send it** — a pencil and a rectangle sit in
  a strip beside the selection, both drawing in red. Whatever is drawn is
  baked into the saved or copied image. `Ctrl+Z` takes back the last shape;
  clicking the armed tool again disarms it and hands the resize handles back.
- **Full-page (scrolling) screenshot** — captures what a region *would* show
  if the window were tall enough. Pick the region, hit **Full page**, and the
  app scrolls it a step at a time, grabs a frame after each one, and stitches
  the frames into a single tall PNG.
  - **Scrolls through UI Automation where it can.** A `ScrollPattern` reports
    the exact scroll position, the share of the document on screen, and takes
    a target to move to — so there is no guessing at how far a wheel notch
    goes, no waiting out a fixed delay for smooth scrolling, and an honest
    signal for "this is the bottom". Chromium, Firefox, WPF, WinForms, UWP and
    Explorer all expose it.
  - **Falls back to the mouse wheel** for apps that paint their own scrolling
    without publishing the pattern. The wheel path calibrates itself on the
    first notch, since apps disagree wildly about how far one goes.
  - **The frames decide where the seam is**, either way: each new frame is
    matched against the last to measure how far the content really moved, so
    sticky headers, footers and the scrollbar don't throw the alignment off.
- **Punto-style layout correction** — manually rewrite the last typed word
  from the wrong keyboard layout into the right one, or let the automatic
  mode detect and fix gibberish in real time as you type, usually within
  three or four keystrokes rather than at the end of the word.
  - **Three languages.** English, Russian and Ukrainian, each with its own
    word list. The same keystrokes have two Cyrillic readings — `s` is `ы`
    in Russian but `і` in Ukrainian — so both are tested and the dictionaries
    decide: `ghbdtn` → `привет`, `ghbdsn` → `привіт`. Ukrainian typed on a
    Russian layout is fixed too (`привыт` → `привіт`), as is Latin typed on a
    Ukrainian one. Ties go to whichever Cyrillic layout you actually use.
  - **Backspace undoes it.** Pressing Backspace immediately after a correction
    restores exactly what you typed, puts your layout back, and blacklists
    that word for the rest of the session, so it stops arguing with you.
  - **Punctuation is applied after the fix, not before.** The key that ends
    the word is held back until the correction lands — so `Enter` sends an
    already-corrected message instead of a corrected one being sent too late.
- **Ask the model** — `Ctrl+Tab` drops a single input line in the middle of the
  screen. Type, press Enter, and the answer unfolds underneath while the input
  stays where it was; the window height follows the reply, so a one-word answer
  doesn't leave a half-empty panel and a long one scrolls. No caption, no
  buttons — Escape dismisses it. Follow-ups keep the thread, and it answers in
  the current UI language, since a hotkey leaves no room to ask for one. The
  conversation lives only as long as the window: reopening starts clean.
- **Taskbar icon centering** — dynamically repositions icons to the middle
  of the taskbar and keeps them centered as icons come and go.
- **System tray** — left-click opens settings, right-click shows a
  "Settings / Exit" menu. The console window is hidden.
- **Multi-language UI** — settings, popup, and tray menu translated into
  13 languages (en, ru, es, fr, de, pt, it, pl, tr, uk, zh, ja, ko).
- **macOS-style interface** — grouped inset cards, switches instead of
  checkboxes, secondary-colour group titles, and a dark title bar. Every
  rounded shape is drawn at 4× into an off-screen buffer and filtered back
  down: GDI has no antialiasing, and a stair-stepped corner is the one thing
  that gives a hand-drawn control away.
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
| `Ctrl+Tab` | Ask the model a question |

Inside the capture overlay, once a region is selected:

| Key | Action |
| --- | --- |
| `Ctrl+C` | Copy the region to the clipboard |
| `Ctrl+S` | Save the region to a file |
| `Ctrl+Z` | Undo the last drawn shape |
| `Esc` | Cancel |

All hotkeys are remappable from the settings window (click the tray icon).

> `Ctrl+Tab` is registered globally, which takes it away from every other
> application while the app runs — browsers and editors included. Remap it in
> settings if you want tab switching back.

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
5. To capture a page that doesn't fit on screen, draw the region over the
   scrollable area and click **Full page**. Keep hands off the mouse and
   keyboard while it scrolls; when it stops, a save dialog offers the
   stitched PNG.
6. To mark something up, pick the pencil or the rectangle from the strip beside
   the selection and drag inside it, then save or copy as usual.
7. Press `Ctrl+Tab`, type a question, press `Enter`. `Shift+Enter` breaks the
   line instead of sending; `Esc` closes the window. Requires a DeepSeek key.
8. Click the tray icon to open settings: remap hotkeys, pick a screenshot
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
├── scroll_capture.rs  # scrolling "full page" capture and frame stitching
├── uia_scroll.rs      # UI Automation scroll driver (wheel is the fallback)
├── screenshot.rs      # pixel capture and PNG encoding
├── ocr.rs             # OCR.space + WinRT OCR
├── translate.rs       # translation policy: DeepSeek or MyMemory, language detection
├── deepseek.rs        # DeepSeek chat-completions client, shared by both callers
├── ask.rs             # "ask the model" chat window
├── popup.rs           # translation result popup window
├── i18n.rs            # 13-language UI string table
├── paint.rs           # antialiased rounded rectangles and arbitrary shapes
├── button.rs          # macOS-style push buttons
├── theme.rs           # macOS dark-appearance colour palette
└── utils.rs           # UTF-16, urlencode, Win32 input helpers
```

---

## Settings location

Settings are persisted to
`%APPDATA%\screen-translator\settings.json`.
Delete the file to reset everything to defaults.

A log of what the app did is written next to it, at
`%APPDATA%\screen-translator\log.txt`, and truncated on every start. It is the
first thing to look at when a capture or a hotkey misbehaves — the scrolling
capture in particular records which scroll driver it chose and how far each
frame moved. Setting `SCROLL_DEBUG_DIR` to a folder additionally dumps every
grabbed frame there as a PNG.

---

## License

MIT
