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
  `deepseek-flash` model is used instead, with automatic fallback to
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
- **Punto-style layout correction** — words typed on the wrong keyboard layout
  are retyped on the right one as you type, and the layout is switched so the
  rest comes out right; clear cases are fixed within four or five keystrokes,
  the rest when the word ends.
  - **Real dictionaries.** A quarter of a million word *forms* per language
    (English, Russian, Ukrainian), ranked by frequency from subtitles and
    Wikipedia, plus a character model for words no list holds. Every word is
    judged by how likely each layout's reading of the same keys is, and what
    you typed gets a head start sized by how much it looks meant: a common
    word (`руку`, `tv`) is never rewritten into another one (`here`, `ем`), a
    typo stays a typo in its own language (`кучча`), and layout noise
    (`ghbdtn`, `руддщ`) loses to any real word.
  - **Russian and Ukrainian told apart by the word.** `ghbdtn` → `привет`,
    `ghbdsn` → `привіт`; Ukrainian typed on the Russian layout is fixed
    (`привыт` → `привіт`), and so is Russian on the Ukrainian one (`єто` →
    `это`). Spellings both languages share follow the language you have been
    writing in, and a correct word on the wrong Cyrillic layout (`купувати` on
    the Russian one) just has the layout switched under it.
  - **Whole words, punctuation included.** `,` `.` `;` `'` `[` `]` are
    letters on the Cyrillic layouts (`б ю ж э х ъ`), so `,jkmibvb` becomes
    `большими`, not `,ольшими`. Punctuation after a word is retyped as the
    target layout has it: `ghbdtn/` becomes `привет.`, `руддщб` becomes `hello,`.
  - **Short words follow the sentence.** Alone, `z`, `,s` or `ye` could be
    anything; once the next word settles which layout you thought you were on,
    they are fixed along with it: `z ,s gjikf` → `я бы пошла`, `ye lf` → `ну да`.
  - **`Pause` takes a correction back** — restores what you typed, switches
    your layout back and remembers the word (`punto_exceptions.txt` next to
    the settings, one word per line, editable). On a word nobody touched it
    converts it instead, and pressing it again goes back. `Ctrl+Alt+L` does the
    same for the word just typed, or converts the selection when there is one.
    Backspace is just Backspace.
  - **Enter and Tab act after the fix.** The key that ends the word is held
    back until the correction lands, so `Enter` sends the corrected message.
  - **Any installed layout variant** — read from Windows itself, so "Russian
    (Ukraine)", Ukrainian (Enhanced) with `ґ` on its own key, or a UK English
    board all work. Classic password boxes are left alone.
- **Ask the model** *(off by default — switch it on in Settings → Ask AI;
  while it's off, its hotkey isn't registered at all)* — `Ctrl+Tab` drops a
  single input line beside the text you
  have selected, or in the middle of the screen when nothing is. Type, press
  Enter, and the answer unfolds underneath while the input stays where it was;
  the window height follows the reply, so a one-word answer doesn't leave a
  half-empty panel and a long one scrolls. No caption, no buttons — drag it
  anywhere by the panel around the fields, and dismiss it with Escape or the
  hotkey again. Clicking into another window leaves it alone, so an answer can
  stay on screen while you work underneath it. Whatever text is selected when
  the hotkey is pressed arrives already in the input, unsent, so a question can
  be asked *about* something without pasting it first. Follow-ups keep the thread, and
  it answers in the current UI language, since a hotkey leaves no room to ask
  for one. The conversation lives only as long as the window: reopening starts
  clean.
  - **It has eyes.** Paste a picture into it and the question is asked about
    that: a thumbnail appears beside the input, and clicking the thumbnail takes
    it back off. With a Claude API key set, the picture is sent to the model as
    a picture — it can read a chart, a diagram, or an interface. Without one,
    the window falls back to OCR: it reads the text out of the image locally and
    hands that to DeepSeek, which covers a screenshot of an error, a document or
    a page of code, but cannot describe what is *drawn*. Which backend answers
    is decided by the picture alone — a question with no image always goes to
    DeepSeek, whether or not the second key is set.
- **Taskbar icon centering** — dynamically repositions icons to the middle
  of the taskbar and keeps them centered as icons come and go.
- **System tray** — its own icon; left-click opens settings, right-click shows
  a dark "Settings / Exit" menu. The console window is hidden.
- **Multi-language UI** — settings, popup, and tray menu translated into
  13 languages (en, ru, es, fr, de, pt, it, pl, tr, uk, zh, ja, ko).
- **Modern dark interface** — a frameless settings window with a navigation
  column (General / Shortcuts / Translation / Ask AI) instead of one long
  scroll; grouped cards, switches, keycap-style shortcut fields, and API keys
  masked until you click the eye, each with a live "works / rejected /
  unreachable" status. The translation popup and dropdowns are floating cards
  with antialiased corners and a soft shadow (per-pixel-alpha layered
  windows); the capture overlay keeps its tools and actions in one bar under
  the selection. Every rounded shape is antialiased — GDI has none of its own,
  and a stair-stepped corner is the one thing that gives a hand-drawn control
  away.
- **Windows autostart** — optional one-click toggle that writes to
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

---

## Default hotkeys

| Hotkey | Action |
| --- | --- |
| `Ctrl+Alt+T` | Translate the currently selected text |
| `Ctrl+Alt+S` | Select a region → OCR → translate |
| `Ctrl+Alt+D` | Screenshot a region to the clipboard |
| `Ctrl+Alt+L` | Switch the layout of the word just typed, or of the selection |
| `Pause` | Take back the last automatic correction (or convert the last word) |
| `Ctrl+Alt+A` | Turn automatic layout correction on or off |
| `Ctrl+Tab` | Ask the model a question (only once turned on in settings) |

Inside the capture overlay, once a region is selected:

| Key | Action |
| --- | --- |
| `Ctrl+C` | Copy the region to the clipboard |
| `Ctrl+S` | Save the region to a file |
| `Ctrl+Z` | Undo the last drawn shape |
| `Esc` | Cancel |

All hotkeys are remappable from the settings window (click the tray icon).

> Once the ask window is on, `Ctrl+Tab` is registered globally, which takes it
> away from every other application while the app runs — browsers and editors
> included. Remap it in settings if you want tab switching back; with the ask
> window off it isn't taken at all.

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
6. To mark something up, pick the pencil or the rectangle at the left of the
   bar under the selection and drag inside it, then save or copy as usual.
7. Turn the ask window on in Settings → Ask AI, then press `Ctrl+Tab`, type a
   question, press `Enter`. Select text first and it
   is waiting in the input when the window opens. `Shift+Enter` breaks the line
   instead of sending; `Esc` or the hotkey again closes the window. `Ctrl+V`
   attaches a picture from the clipboard instead of pasting text. Requires a
   DeepSeek key; a Claude key is what lets it see the picture rather than just
   read the text out of it.
8. Click the tray icon to open settings: remap hotkeys, pick a screenshot
   folder, toggle Punto / taskbar centering / autostart / the ask window.
   `Tab` moves between fields, `Ctrl+Tab` between pages, `Enter` saves,
   `Esc` closes.

---

## Chrome extension: full-page screenshot, save image as PNG / JPG / WebP

The extension in `chrome_extension/` does two things.

**Screenshot the whole page** — the toolbar button, `Ctrl+Shift+S`, or
**Screenshot the whole page** in the page's right-click menu. The page is
captured top to bottom in one go, wherever you are scrolled, without
scrolling through it: Chrome renders the parts that are off screen itself
(DevTools protocol, `captureBeyondViewport`). A long page takes about a
second. The PNG goes straight to Downloads, named after the page title.

- While it runs (a second or so) Chrome shows its *"… started debugging this
  browser"* bar. That's how Chrome labels any use of the protocol, and it
  can't be switched off from an extension.
- Sticky headers come out once, at the top: the page is put at the top for
  the capture — instantly, not scrolled — and put back afterwards.
- Images marked `loading="lazy"` are loaded before the capture. Content a
  script only adds as you scroll (infinite feeds) isn't there to capture.
- Pages that scroll an inner panel instead of the page itself (web mail,
  chat apps) come out as one screen; the exe's **Full page** button handles
  those by scrolling.
- Pages taller than 32 000 px are cut there — the largest canvas Chrome
  will encode.
- `chrome://` pages and the Web Store can't be captured by any extension.

**Save image as PNG / JPG / WebP.** Chrome's own **Save image as…** writes
whatever the site served — mostly WebP or AVIF — and the type box in its dialog
can't change that. The extension adds a **Save image as → PNG… / JPG… /
WebP…** submenu to the right-click menu on images. It re-encodes the picture
and then opens the ordinary Save As dialog with the right extension already in
the file name.

It is separate from the exe; no program outside the browser can add entries to
that menu, or render a page it hasn't painted.

**Install** (Chrome or Edge):

1. Open `chrome://extensions` (`edge://extensions`) and turn on
   **Developer mode**.
2. Click **Load unpacked** and pick the `chrome_extension` folder. Chrome loads
   it from there, so leave the folder where it is. After updating the folder,
   press the reload arrow on the extension's card.
3. If `Ctrl+Shift+S` is taken by something else, pick another key at
   `chrome://extensions/shortcuts`.

**Behaviour worth knowing:**

- JPG is written at quality 92; transparent areas become white, since JPEG has
  no alpha. PNG and WebP keep transparency.
- Animated GIF and WebP are saved as their first frame. SVG is rasterised at
  its own declared size.
- The image is fetched again by the extension, which is why it asks for access
  to all sites: pictures usually live on a CDN, not on the page's domain.
  Hotlink-protected images and `blob:` images are read from inside the page
  instead.
- If the image can't be fetched or decoded, the page shows an alert saying so.

---

## Dependencies

- [`windows`](https://crates.io/crates/windows) — Win32 API bindings
- [`arboard`](https://crates.io/crates/arboard) — clipboard access
- [`ureq`](https://crates.io/crates/ureq) — HTTP client
- [`serde`](https://crates.io/crates/serde) + `serde_json` — settings persistence
- [`base64`](https://crates.io/crates/base64) — image encoding for OCR.space
  and for the vision API
- [`chrono`](https://crates.io/crates/chrono) — screenshot filename timestamps
- [`anyhow`](https://crates.io/crates/anyhow) — error handling

External services used:
- MyMemory Translation API (free, no key required) — default translator
- DeepSeek Chat Completions API (paid, optional) — higher-quality
  alternative; configure the key in the settings window
- Anthropic Messages API (paid, optional) — what the ask window uses to look
  at an attached picture; without a key it falls back to OCR
- OCR.space Parse Image API (free, optional key)

---

## Project layout

```
src/
├── main.rs            # entry point, main message loop, hotkey dispatch
├── settings.rs        # settings model, JSON load/save
├── settings_ui.rs     # frameless dark settings window with page navigation
├── tray.rs            # system tray icon (Shell_NotifyIcon)
├── autostart.rs       # Windows Registry Run key for autostart
├── autotype/          # Punto-style layout correction
│   ├── mod.rs         #   keyboard/mouse hooks on their own thread, SendInput
│   ├── engine.rs      #   keystrokes in, corrections out (no Win32; simulated in tests)
│   ├── decide.rs      #   which layout a word was meant for
│   ├── model.rs       #   dictionaries and character trigram models
│   ├── keymap.rs      #   what each key types on each installed layout
│   └── eval.rs        #   accuracy measurement on held-out word lists
├── taskbar_center.rs  # taskbar icon centering
├── capture.rs         # rectangular screen-region selector overlay
├── scroll_capture.rs  # scrolling "full page" capture and frame stitching
├── uia_scroll.rs      # UI Automation scroll driver (wheel is the fallback)
├── screenshot.rs      # pixel capture and PNG encoding
├── ocr.rs             # OCR.space + WinRT OCR
├── translate.rs       # translation policy: DeepSeek or MyMemory, language detection
├── deepseek.rs        # DeepSeek chat-completions client, shared by both callers
├── vision.rs          # Anthropic messages client — the ask window's eyes
├── ask.rs             # "ask the model" chat window
├── popup.rs           # translation result popup window
├── i18n.rs            # 13-language UI string table
├── paint.rs           # antialiased rounded rectangles and arbitrary shapes
├── button.rs          # macOS-style push buttons
├── theme.rs           # macOS dark-appearance colour palette
└── utils.rs           # UTF-16, urlencode, Win32 input helpers

data/punto/            # word models, built by tools/build_punto_dicts.py
tools/                 # the dictionary builder (downloads its corpora)

chrome_extension/      # "Save image as PNG / JPG / WebP" for Chrome and Edge
├── manifest.json
├── background.js      # context menu, fetch fallbacks, Save As dialog
├── offscreen.js       # decode and re-encode; owns the blob: URLs
├── offscreen.html
└── _locales/          # en, ru, uk
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
