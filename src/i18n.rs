use std::sync::Mutex;

// ============================================================
// Supported languages (English is the default and fallback).
// ============================================================

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Language {
    En, Ru, Es, Fr, De, Pt, It, Pl, Tr, Uk, Zh, Ja, Ko,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en", Self::Ru => "ru", Self::Es => "es", Self::Fr => "fr",
            Self::De => "de", Self::Pt => "pt", Self::It => "it", Self::Pl => "pl",
            Self::Tr => "tr", Self::Uk => "uk", Self::Zh => "zh", Self::Ja => "ja",
            Self::Ko => "ko",
        }
    }

    pub fn from_code(code: &str) -> Self {
        match code {
            "ru" => Self::Ru, "es" => Self::Es, "fr" => Self::Fr, "de" => Self::De,
            "pt" => Self::Pt, "it" => Self::It, "pl" => Self::Pl, "tr" => Self::Tr,
            "uk" => Self::Uk, "zh" => Self::Zh, "ja" => Self::Ja, "ko" => Self::Ko,
            _ => Self::En,
        }
    }

    /// Name shown in the language picker (in the language itself).
    pub fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ru => "Русский",
            Self::Es => "Español",
            Self::Fr => "Français",
            Self::De => "Deutsch",
            Self::Pt => "Português",
            Self::It => "Italiano",
            Self::Pl => "Polski",
            Self::Tr => "Türkçe",
            Self::Uk => "Українська",
            Self::Zh => "中文",
            Self::Ja => "日本語",
            Self::Ko => "한국어",
        }
    }

    pub fn all() -> &'static [Language] {
        &[
            Self::En, Self::Ru, Self::Es, Self::Fr, Self::De, Self::Pt, Self::It,
            Self::Pl, Self::Tr, Self::Uk, Self::Zh, Self::Ja, Self::Ko,
        ]
    }
}

// ============================================================
// Current language (shared state).
// ============================================================

static CURRENT: Mutex<Language> = Mutex::new(Language::En);

pub fn set(lang: Language) {
    *CURRENT.lock().unwrap() = lang;
}

pub fn current() -> Language {
    *CURRENT.lock().unwrap()
}

pub fn set_from_code(code: &str) {
    set(Language::from_code(code));
}

// ============================================================
// Translation table.
// One row per key; each language column holds the native string.
// English is the fallback if a key isn't found.
// ============================================================

struct Entry {
    key: &'static str,
    en: &'static str, ru: &'static str, es: &'static str, fr: &'static str,
    de: &'static str, pt: &'static str, it: &'static str, pl: &'static str,
    tr: &'static str, uk: &'static str, zh: &'static str, ja: &'static str,
    ko: &'static str,
}

static ENTRIES: &[Entry] = &[
    Entry {
        key: "settings.title",
        en: "Settings", ru: "Настройки", es: "Configuración", fr: "Paramètres",
        de: "Einstellungen", pt: "Configurações", it: "Impostazioni",
        pl: "Ustawienia", tr: "Ayarlar", uk: "Налаштування",
        zh: "设置", ja: "設定", ko: "설정",
    },
    Entry {
        key: "settings.section.general",
        en: "GENERAL", ru: "ОБЩИЕ", es: "GENERAL", fr: "GÉNÉRAL",
        de: "ALLGEMEIN", pt: "GERAL", it: "GENERALE",
        pl: "OGÓLNE", tr: "GENEL", uk: "ЗАГАЛЬНІ",
        zh: "常规", ja: "一般", ko: "일반",
    },
    Entry {
        key: "settings.section.hotkeys",
        en: "HOTKEYS", ru: "ГОРЯЧИЕ КЛАВИШИ", es: "ATAJOS DE TECLADO", fr: "RACCOURCIS",
        de: "TASTENKOMBINATIONEN", pt: "ATALHOS", it: "SCORCIATOIE",
        pl: "SKRÓTY", tr: "KISAYOLLAR", uk: "ГАРЯЧІ КЛАВІШІ",
        zh: "快捷键", ja: "ショートカット", ko: "단축키",
    },
    Entry {
        key: "settings.section.folder",
        en: "SCREENSHOT FOLDER", ru: "ПАПКА СКРИНШОТОВ", es: "CARPETA DE CAPTURAS",
        fr: "DOSSIER DE CAPTURES", de: "SCREENSHOT-ORDNER", pt: "PASTA DE CAPTURAS",
        it: "CARTELLA SCREENSHOT", pl: "FOLDER ZRZUTÓW",
        tr: "EKRAN GÖRÜNTÜ KLASÖRÜ", uk: "ПАПКА СКРІНШОТІВ",
        zh: "截图文件夹", ja: "スクリーンショットフォルダ", ko: "스크린샷 폴더",
    },
    Entry {
        key: "settings.section.functions",
        en: "FEATURES", ru: "ФУНКЦИИ", es: "FUNCIONES", fr: "FONCTIONS",
        de: "FUNKTIONEN", pt: "FUNÇÕES", it: "FUNZIONI",
        pl: "FUNKCJE", tr: "ÖZELLİKLER", uk: "ФУНКЦІЇ",
        zh: "功能", ja: "機能", ko: "기능",
    },
    Entry {
        key: "settings.label.language",
        en: "Interface language:", ru: "Язык интерфейса:",
        es: "Idioma de la interfaz:", fr: "Langue de l'interface :",
        de: "Sprache:", pt: "Idioma da interface:",
        it: "Lingua dell'interfaccia:", pl: "Język interfejsu:",
        tr: "Arayüz dili:", uk: "Мова інтерфейсу:",
        zh: "界面语言：", ja: "言語：", ko: "인터페이스 언어:",
    },
    Entry {
        key: "settings.hotkey.translate",
        en: "Translate text:", ru: "Перевод текста:",
        es: "Traducir texto:", fr: "Traduire le texte :",
        de: "Text übersetzen:", pt: "Traduzir texto:",
        it: "Traduci testo:", pl: "Tłumacz tekst:",
        tr: "Metni çevir:", uk: "Перекласти текст:",
        zh: "翻译文本：", ja: "テキスト翻訳：", ko: "텍스트 번역:",
    },
    Entry {
        key: "settings.hotkey.ocr",
        en: "OCR + translate:", ru: "OCR + перевод:",
        es: "OCR + traducir:", fr: "OCR + traduire :",
        de: "OCR + Übersetzen:", pt: "OCR + traduzir:",
        it: "OCR + traduci:", pl: "OCR + tłumacz:",
        tr: "OCR + çevir:", uk: "OCR + переклад:",
        zh: "OCR + 翻译：", ja: "OCR + 翻訳：", ko: "OCR + 번역:",
    },
    Entry {
        key: "settings.hotkey.screenshot",
        en: "Screenshot:", ru: "Скриншот:",
        es: "Captura:", fr: "Capture d'écran :",
        de: "Screenshot:", pt: "Captura:",
        it: "Screenshot:", pl: "Zrzut ekranu:",
        tr: "Ekran görüntüsü:", uk: "Скріншот:",
        zh: "截图：", ja: "スクリーンショット：", ko: "스크린샷:",
    },
    Entry {
        key: "settings.hotkey.layout",
        en: "Layout switch:", ru: "Смена раскладки:",
        es: "Cambiar disposición:", fr: "Changer disposition :",
        de: "Tastaturlayout wechseln:", pt: "Trocar layout:",
        it: "Cambia layout:", pl: "Zmień układ:",
        tr: "Düzen değiştir:", uk: "Зміна розкладки:",
        zh: "切换布局：", ja: "レイアウト切替：", ko: "레이아웃 전환:",
    },
    Entry {
        key: "settings.hotkey.explorer_cmd",
        en: "Open CMD in folder:", ru: "Открыть CMD в папке:",
        es: "Abrir CMD en carpeta:", fr: "Ouvrir CMD dans dossier :",
        de: "CMD im Ordner öffnen:", pt: "Abrir CMD na pasta:",
        it: "Apri CMD nella cartella:", pl: "Otwórz CMD w folderze:",
        tr: "Klasörde CMD aç:", uk: "Відкрити CMD у папці:",
        zh: "在文件夹中打开 CMD：", ja: "フォルダで CMD を開く：",
        ko: "폴더에서 CMD 열기:",
    },
    Entry {
        key: "settings.checkbox.punto",
        en: "Auto layout correction (Punto Switcher)",
        ru: "Авто-смена раскладки (Punto Switcher)",
        es: "Autocorrección de disposición (Punto Switcher)",
        fr: "Correction auto de disposition (Punto Switcher)",
        de: "Auto-Layout-Korrektur (Punto Switcher)",
        pt: "Correção automática de layout (Punto Switcher)",
        it: "Correzione automatica layout (Punto Switcher)",
        pl: "Automatyczna korekta układu (Punto Switcher)",
        tr: "Otomatik düzen düzeltme (Punto Switcher)",
        uk: "Авто-зміна розкладки (Punto Switcher)",
        zh: "自动布局校正 (Punto Switcher)",
        ja: "レイアウト自動補正 (Punto Switcher)",
        ko: "자동 레이아웃 교정 (Punto Switcher)",
    },
    Entry {
        key: "settings.checkbox.taskbar",
        en: "Center taskbar icons",
        ru: "Центрирование иконок панели задач",
        es: "Centrar iconos de la barra de tareas",
        fr: "Centrer les icônes de la barre des tâches",
        de: "Taskleisten-Symbole zentrieren",
        pt: "Centralizar ícones da barra de tarefas",
        it: "Centrare le icone della barra delle applicazioni",
        pl: "Wyśrodkuj ikony paska zadań",
        tr: "Görev çubuğu simgelerini ortala",
        uk: "Центрування іконок панелі завдань",
        zh: "居中任务栏图标",
        ja: "タスクバーアイコンを中央揃え",
        ko: "작업 표시줄 아이콘 가운데 정렬",
    },
    Entry {
        key: "settings.checkbox.autostart",
        en: "Start with Windows",
        ru: "Запускать при старте Windows",
        es: "Iniciar con Windows",
        fr: "Lancer avec Windows",
        de: "Mit Windows starten",
        pt: "Iniciar com o Windows",
        it: "Avvia con Windows",
        pl: "Uruchom z systemem Windows",
        tr: "Windows ile başlat",
        uk: "Запускати зі стартом Windows",
        zh: "随 Windows 启动",
        ja: "Windows 起動時に開始",
        ko: "Windows 시작 시 실행",
    },
    Entry {
        key: "settings.checkbox.explorer_cmd",
        en: "\"Open CMD here\" in Explorer context menu",
        ru: "Пункт «Открыть CMD здесь» в меню проводника",
        es: "«Abrir CMD aquí» en el menú del Explorador",
        fr: "« Ouvrir CMD ici » dans le menu de l'Explorateur",
        de: "„CMD hier öffnen“ im Explorer-Kontextmenü",
        pt: "\"Abrir CMD aqui\" no menu do Explorador",
        it: "\"Apri CMD qui\" nel menu di Esplora risorse",
        pl: "„Otwórz CMD tutaj” w menu Eksploratora",
        tr: "Gezgin menüsünde \"CMD'yi burada aç\"",
        uk: "Пункт «Відкрити CMD тут» у меню провідника",
        zh: "在资源管理器菜单中添加\"在此处打开 CMD\"",
        ja: "エクスプローラーメニューに「ここで CMD を開く」",
        ko: "탐색기 메뉴에 \"여기서 CMD 열기\"",
    },
    Entry {
        key: "settings.btn.save",
        en: "Save", ru: "Сохранить", es: "Guardar", fr: "Enregistrer",
        de: "Speichern", pt: "Salvar", it: "Salva",
        pl: "Zapisz", tr: "Kaydet", uk: "Зберегти",
        zh: "保存", ja: "保存", ko: "저장",
    },
    Entry {
        key: "settings.btn.browse",
        en: "Browse...", ru: "Обзор...", es: "Examinar...", fr: "Parcourir...",
        de: "Durchsuchen...", pt: "Procurar...", it: "Sfoglia...",
        pl: "Przeglądaj...", tr: "Gözat...", uk: "Огляд...",
        zh: "浏览...", ja: "参照...", ko: "찾아보기...",
    },
    Entry {
        key: "settings.btn.cancel",
        en: "Cancel", ru: "Отмена", es: "Cancelar", fr: "Annuler",
        de: "Abbrechen", pt: "Cancelar", it: "Annulla",
        pl: "Anuluj", tr: "İptal", uk: "Скасувати",
        zh: "取消", ja: "キャンセル", ko: "취소",
    },
    Entry {
        key: "tray.menu.settings",
        en: "Settings", ru: "Настройки", es: "Configuración", fr: "Paramètres",
        de: "Einstellungen", pt: "Configurações", it: "Impostazioni",
        pl: "Ustawienia", tr: "Ayarlar", uk: "Налаштування",
        zh: "设置", ja: "設定", ko: "설정",
    },
    Entry {
        key: "tray.menu.exit",
        en: "Exit", ru: "Выход", es: "Salir", fr: "Quitter",
        de: "Beenden", pt: "Sair", it: "Esci",
        pl: "Wyjście", tr: "Çıkış", uk: "Вихід",
        zh: "退出", ja: "終了", ko: "종료",
    },
    Entry {
        key: "popup.translating",
        en: "Translating...", ru: "Переводим...", es: "Traduciendo...",
        fr: "Traduction...", de: "Übersetzen...", pt: "Traduzindo...",
        it: "Traduzione...", pl: "Tłumaczenie...", tr: "Çeviriliyor...",
        uk: "Переклад...", zh: "翻译中...", ja: "翻訳中...", ko: "번역 중...",
    },
    Entry {
        key: "popup.recognizing",
        en: "Recognizing text...", ru: "Распознаём текст...",
        es: "Reconociendo texto...", fr: "Reconnaissance du texte...",
        de: "Text wird erkannt...", pt: "Reconhecendo texto...",
        it: "Riconoscimento testo...", pl: "Rozpoznawanie tekstu...",
        tr: "Metin tanınıyor...", uk: "Розпізнавання тексту...",
        zh: "识别文字中...", ja: "テキスト認識中...", ko: "텍스트 인식 중...",
    },
    Entry {
        key: "popup.no_text",
        en: "No text recognized in the selected area",
        ru: "Текст не распознан в выбранной области",
        es: "No se reconoció texto en el área seleccionada",
        fr: "Aucun texte reconnu dans la zone sélectionnée",
        de: "Kein Text im ausgewählten Bereich erkannt",
        pt: "Nenhum texto reconhecido na área selecionada",
        it: "Nessun testo riconosciuto nell'area selezionata",
        pl: "Nie rozpoznano tekstu w wybranym obszarze",
        tr: "Seçilen alanda metin tanınmadı",
        uk: "Текст не розпізнано у вибраній області",
        zh: "在所选区域中未识别到文字",
        ja: "選択した領域にテキストが見つかりません",
        ko: "선택한 영역에서 텍스트를 인식하지 못했습니다",
    },
    Entry {
        key: "popup.screenshot_copied",
        en: "Screenshot copied", ru: "Скриншот скопирован",
        es: "Captura copiada", fr: "Capture copiée",
        de: "Screenshot kopiert", pt: "Captura copiada",
        it: "Screenshot copiato", pl: "Zrzut skopiowany",
        tr: "Ekran görüntüsü kopyalandı", uk: "Скріншот скопійовано",
        zh: "截图已复制", ja: "スクリーンショットをコピーしました",
        ko: "스크린샷이 복사되었습니다",
    },
    Entry {
        key: "popup.screenshot_saved",
        en: "Screenshot saved", ru: "Скриншот сохранён",
        es: "Captura guardada", fr: "Capture enregistrée",
        de: "Screenshot gespeichert", pt: "Captura salva",
        it: "Screenshot salvato", pl: "Zrzut zapisany",
        tr: "Ekran görüntüsü kaydedildi", uk: "Скріншот збережено",
        zh: "截图已保存", ja: "スクリーンショットを保存しました",
        ko: "스크린샷이 저장되었습니다",
    },
    Entry {
        key: "popup.error_prefix",
        en: "Error: ", ru: "Ошибка: ", es: "Error: ", fr: "Erreur : ",
        de: "Fehler: ", pt: "Erro: ", it: "Errore: ",
        pl: "Błąd: ", tr: "Hata: ", uk: "Помилка: ",
        zh: "错误：", ja: "エラー：", ko: "오류: ",
    },
    Entry {
        key: "popup.ocr_error_prefix",
        en: "OCR error: ", ru: "Ошибка OCR: ",
        es: "Error OCR: ", fr: "Erreur OCR : ",
        de: "OCR-Fehler: ", pt: "Erro de OCR: ",
        it: "Errore OCR: ", pl: "Błąd OCR: ",
        tr: "OCR hatası: ", uk: "Помилка OCR: ",
        zh: "OCR 错误：", ja: "OCR エラー：", ko: "OCR 오류: ",
    },
    Entry {
        key: "popup.autotype_on",
        en: "Auto layout correction enabled",
        ru: "Авто-смена раскладки включена",
        es: "Corrección automática de disposición activada",
        fr: "Correction auto de disposition activée",
        de: "Auto-Layout-Korrektur aktiviert",
        pt: "Correção automática de layout ativada",
        it: "Correzione automatica layout attivata",
        pl: "Autokorekta układu włączona",
        tr: "Otomatik düzen düzeltme açık",
        uk: "Авто-зміну розкладки увімкнено",
        zh: "已启用自动布局校正",
        ja: "レイアウト自動補正を有効化",
        ko: "자동 레이아웃 교정 활성화됨",
    },
    Entry {
        key: "popup.autotype_off",
        en: "Auto layout correction disabled",
        ru: "Авто-смена раскладки выключена",
        es: "Corrección automática de disposición desactivada",
        fr: "Correction auto de disposition désactivée",
        de: "Auto-Layout-Korrektur deaktiviert",
        pt: "Correção automática de layout desativada",
        it: "Correzione automatica layout disattivata",
        pl: "Autokorekta układu wyłączona",
        tr: "Otomatik düzen düzeltme kapalı",
        uk: "Авто-зміну розкладки вимкнено",
        zh: "已禁用自动布局校正",
        ja: "レイアウト自動補正を無効化",
        ko: "자동 레이아웃 교정 비활성화됨",
    },
    Entry {
        key: "popup.taskbar_on",
        en: "Taskbar icon centering ON",
        ru: "Центрирование иконок таскбара ВКЛ",
        es: "Centrado de iconos de barra activado",
        fr: "Centrage des icônes activé",
        de: "Taskleisten-Zentrierung EIN",
        pt: "Centralização de ícones ativada",
        it: "Centratura icone barra ATTIVA",
        pl: "Wyśrodkowanie ikon paska WŁ.",
        tr: "Görev çubuğu ortalama AÇIK",
        uk: "Центрування іконок УВІМК",
        zh: "任务栏居中 开",
        ja: "タスクバー中央揃え ON",
        ko: "작업 표시줄 가운데 정렬 켜짐",
    },
    Entry {
        key: "popup.taskbar_off",
        en: "Taskbar icon centering OFF",
        ru: "Центрирование иконок таскбара ВЫКЛ",
        es: "Centrado de iconos de barra desactivado",
        fr: "Centrage des icônes désactivé",
        de: "Taskleisten-Zentrierung AUS",
        pt: "Centralização de ícones desativada",
        it: "Centratura icone barra DISATTIVA",
        pl: "Wyśrodkowanie ikon paska WYŁ.",
        tr: "Görev çubuğu ortalama KAPALI",
        uk: "Центрування іконок ВИМК",
        zh: "任务栏居中 关",
        ja: "タスクバー中央揃え OFF",
        ko: "작업 표시줄 가운데 정렬 꺼짐",
    },
    Entry {
        key: "capture.btn.translate",
        en: "Translate", ru: "Перевод", es: "Traducir", fr: "Traduire",
        de: "Übersetzen", pt: "Traduzir", it: "Traduci",
        pl: "Tłumacz", tr: "Çevir", uk: "Переклад",
        zh: "翻译", ja: "翻訳", ko: "번역",
    },
    Entry {
        key: "capture.btn.save",
        en: "Save", ru: "Сохранить", es: "Guardar", fr: "Enregistrer",
        de: "Speichern", pt: "Salvar", it: "Salva",
        pl: "Zapisz", tr: "Kaydet", uk: "Зберегти",
        zh: "保存", ja: "保存", ko: "저장",
    },
    Entry {
        key: "capture.hint.copy",
        en: "Ctrl+C \u{2014} copy", ru: "Ctrl+C \u{2014} копировать",
        es: "Ctrl+C \u{2014} copiar", fr: "Ctrl+C \u{2014} copier",
        de: "Ctrl+C \u{2014} kopieren", pt: "Ctrl+C \u{2014} copiar",
        it: "Ctrl+C \u{2014} copia", pl: "Ctrl+C \u{2014} kopiuj",
        tr: "Ctrl+C \u{2014} kopyala", uk: "Ctrl+C \u{2014} копіювати",
        zh: "Ctrl+C \u{2014} 复制", ja: "Ctrl+C \u{2014} コピー",
        ko: "Ctrl+C \u{2014} 복사",
    },
    Entry {
        key: "capture.hint.initial",
        en: "Select a screen area",
        ru: "Выделите область экрана",
        es: "Seleccione un área de pantalla",
        fr: "Sélectionnez une zone d'écran",
        de: "Bildschirmbereich auswählen",
        pt: "Selecione uma área da tela",
        it: "Seleziona un'area dello schermo",
        pl: "Zaznacz obszar ekranu",
        tr: "Bir ekran alanı seçin",
        uk: "Виділіть область екрана",
        zh: "选择屏幕区域",
        ja: "画面の領域を選択",
        ko: "화면 영역을 선택하세요",
    },
];

// ============================================================
// Public lookup.
// ============================================================

pub fn t(key: &str) -> &'static str {
    let lang = current();
    for e in ENTRIES {
        if e.key == key {
            return match lang {
                Language::En => e.en, Language::Ru => e.ru,
                Language::Es => e.es, Language::Fr => e.fr,
                Language::De => e.de, Language::Pt => e.pt,
                Language::It => e.it, Language::Pl => e.pl,
                Language::Tr => e.tr, Language::Uk => e.uk,
                Language::Zh => e.zh, Language::Ja => e.ja,
                Language::Ko => e.ko,
            };
        }
    }
    ""
}
