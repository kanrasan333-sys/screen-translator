# Screen Translator

Легковесная утилита для Windows: перевод выделенного текста и распознавание
текста с экрана (OCR) по горячим клавишам. Работает в фоне из системного
трея, без открытых окон.

A lightweight Windows background utility for translating selected text and
running OCR on screen regions via global hotkeys. Lives in the system tray.

---

## Возможности / Features

- **Перевод выделенного текста** — копирует выделение в буфер обмена,
  переводит через [MyMemory](https://mymemory.translated.net/) и показывает
  всплывающее окно рядом с курсором. Направление RU ↔ EN определяется
  автоматически.
- **OCR области экрана** — рисуете прямоугольник мышью, содержимое
  распознаётся через [OCR.space](https://ocr.space/) (основной движок) с
  откатом на Windows WinRT OCR, затем переводится.
- **Скриншот области** — захват выделенной области прямо в буфер обмена.
- **Смена раскладки (Punto)** — вручную переписывает последнее введённое
  слово из ошибочной раскладки в правильную (`привет` ⇄ `ghbdtn`).
  Автоматический режим отслеживает ввод в реальном времени.
- **Центрирование иконок панели задач** — динамически держит иконки
  по центру таскбара, освобождая место у края экрана.
- **Системный трей** — окно настроек открывается кликом по иконке,
  правый клик даёт меню «Настройки / Выход».
- **Автозагрузка Windows** — опционально добавляет приложение в
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

---

## Горячие клавиши по умолчанию / Default hotkeys

| Клавиша / Hotkey | Действие / Action |
| --- | --- |
| `Ctrl+Alt+T` | Перевести выделенный текст |
| `Ctrl+Alt+S` | Выделить область → OCR → перевод |
| `Ctrl+Alt+D` | Скриншот области в буфер |
| `Ctrl+Alt+L` | Смена раскладки последнего слова |

Все хоткеи меняются через окно настроек (клик по иконке в трее).

---

## Сборка / Build

Требуется Rust **edition 2024** (stable 1.85+) и Windows 10/11.

```sh
cargo build --release
```

Бинарник появится в `target/release/screen-translator.exe`.

### OCR.space API key

По умолчанию используется публичный демо-ключ `helloworld` с очень низкими
лимитами. Получите бесплатный ключ на <https://ocr.space/ocrapi> и
подставьте его при сборке:

```sh
# PowerShell
$env:OCR_SPACE_API_KEY = "your_key_here"; cargo build --release

# cmd
set OCR_SPACE_API_KEY=your_key_here && cargo build --release

# bash / Git Bash
OCR_SPACE_API_KEY=your_key_here cargo build --release
```

Если ключ не задан и демо-квота исчерпана — автоматически используется
встроенный Windows OCR (требует установленных языковых пакетов).

---

## Использование / Usage

1. Запустите `screen-translator.exe` — иконка появится в системном трее
   (возле часов, может быть спрятана под стрелкой «показать скрытые значки»).
2. Выделите текст в любом приложении и нажмите `Ctrl+Alt+T` — перевод
   покажется во всплывающем окне.
3. Для OCR нажмите `Ctrl+Alt+S`, выделите область экрана мышью —
   распознанный и переведённый текст появится во всплывающем окне.
4. Кликните иконку в трее для настроек: хоткеи, папка скриншотов,
   включение Punto / центрирования таскбара / автозагрузки.

---

## Зависимости / Dependencies

- [`windows`](https://crates.io/crates/windows) — Win32 API bindings
- [`arboard`](https://crates.io/crates/arboard) — clipboard access
- [`ureq`](https://crates.io/crates/ureq) — HTTP client
- [`serde`](https://crates.io/crates/serde) + `serde_json` — settings persistence
- [`base64`](https://crates.io/crates/base64) — image encoding for OCR.space
- [`chrono`](https://crates.io/crates/chrono) — screenshot filename timestamps
- [`anyhow`](https://crates.io/crates/anyhow) — error handling

Внешние API:
- MyMemory Translation API (бесплатный, без ключа)
- OCR.space Parse Image API (бесплатный, опциональный ключ)

---

## Структура проекта / Project layout

```
src/
├── main.rs            # точка входа, главный цикл сообщений, хоткеи
├── settings.rs        # модель и загрузка/сохранение настроек
├── settings_ui.rs     # кастомное окно настроек (owner-drawn)
├── tray.rs            # системный трей (Shell_NotifyIcon)
├── autostart.rs       # запись в HKCU Run для автозапуска
├── autotype.rs        # авто-смена раскладки (Punto)
├── layout.rs          # карта раскладок RU ↔ EN для Punto
├── taskbar_center.rs  # центрирование иконок таскбара
├── capture.rs         # выделение прямоугольной области экрана
├── screenshot.rs      # захват пикселей и кодирование PNG
├── ocr.rs             # OCR.space + WinRT OCR
├── translate.rs       # MyMemory API
├── popup.rs           # всплывающее окно с переводом
├── theme.rs           # цветовая палитра тёмной темы
└── utils.rs           # UTF-16, urlencode, Win32 input helpers
```

---

## Настройки / Settings location

Настройки сохраняются в
`%APPDATA%\screen-translator\settings.json`.
Удалите файл, чтобы сбросить к умолчаниям.

---

## Лицензия / License

MIT
