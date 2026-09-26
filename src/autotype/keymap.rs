//! Keyboard layouts as the switcher sees them: what each physical key types.
//!
//! Keys are identified by scan code, not virtual key. The virtual key already
//! depends on the layout — which is exactly the thing in question — while the
//! scan code is the key under the finger. Rendering a word's keys through two
//! layouts is then just two table lookups.
//!
//! At runtime the tables come from the layouts actually installed, read with
//! `ToUnicodeEx`, so every variant (Ukrainian Enhanced with `ґ` on the extra
//! key, Russian Typewriter, a UK English board) is handled as it really is.
//! A layout's *language* is read off the same table rather than its language
//! ID: "Russian (Ukraine)" carries a transient ID of 0x2000, and treating that
//! as "not Russian" made the old code believe it was an English layout.

/// Languages the switcher has word models for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    Ru,
    Uk,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Ru, Lang::Uk];

    pub fn is_cyrillic(self) -> bool {
        !matches!(self, Lang::En)
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
            Lang::Uk => "uk",
        }
    }
}

/// One keystroke: the physical key and the case modifiers it was typed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub sc: u8,
    pub shift: bool,
    pub caps: bool,
}

/// Scan codes covered: the whole alphanumeric block plus the ISO extra key.
pub const SCAN_CODES: usize = 0x60;

/// Scan codes of the 26 letter keys of a QWERTY board.
const LETTER_KEYS: [u8; 26] = [
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, // q..p
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, // a..l
    0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, // z..m
];

#[derive(Clone, Debug)]
pub struct KeyMap {
    /// `None` for layouts we have no words for: the switcher stays out of
    /// the way there instead of guessing.
    pub lang: Option<Lang>,
    chars: [[Option<char>; 2]; SCAN_CODES],
}

impl KeyMap {
    /// What `key` types on this layout. CapsLock inverts Shift for letters
    /// only, as the layouts themselves define it.
    pub fn render(&self, key: Key) -> Option<char> {
        let [plain, shifted] = *self.chars.get(key.sc as usize)?;
        let base = plain.or(shifted)?;
        let upper = if base.is_alphabetic() {
            key.shift ^ key.caps
        } else {
            key.shift
        };
        if upper { shifted } else { plain }
    }

    /// All of `keys` on this layout, or `None` if any of them types nothing.
    pub fn render_all(&self, keys: &[Key]) -> Option<String> {
        keys.iter().map(|&k| self.render(k)).collect()
    }

    /// Whether the key types part of a word here: a letter, or the apostrophe
    /// that sits inside words like "don't" and "м'ясо".
    pub fn is_word_key(&self, sc: u8) -> bool {
        self.chars
            .get(sc as usize)
            .is_some_and(|pair| pair.iter().flatten().any(|&c| c.is_alphabetic() || c == '\''))
    }

    /// The keystroke that types `ch` on this layout, if any.
    pub fn key_for(&self, ch: char) -> Option<Key> {
        for (sc, pair) in self.chars.iter().enumerate() {
            for (shift, c) in pair.iter().enumerate() {
                if *c == Some(ch) {
                    return Some(Key {
                        sc: sc as u8,
                        shift: shift == 1,
                        caps: false,
                    });
                }
            }
        }
        None
    }

    /// The language this table types, judged by the keys themselves.
    ///
    /// Russian and Ukrainian are recognised by the letters that differ between
    /// them, plus enough of the standard arrangement to rule out the other
    /// Cyrillic layouts (Belarusian puts `ў` where Ukrainian has `ї`; Kazakh
    /// moves letters onto the digit row). A Latin table only counts as English
    /// when the layout says it is: a German or Polish board would have its
    /// words judged against an English dictionary otherwise.
    fn classify(&self, lang_id: u16) -> Option<Lang> {
        let plain = |sc: usize| self.chars[sc][0];
        let standard = plain(0x02) == Some('1') && plain(0x10) == Some('й');
        match plain(0x1F) {
            Some('ы') if standard => Some(Lang::Ru),
            Some('і') if standard && plain(0x1B) == Some('ї') => Some(Lang::Uk),
            Some(_) => {
                let latin = LETTER_KEYS
                    .iter()
                    .filter(|&&sc| plain(sc as usize).is_some_and(|c| c.is_ascii_lowercase()))
                    .count();
                let primary = lang_id & 0x3FF;
                // 0x09 is English; 0 is a transient ID ("Russian (Ukraine)"
                // style), which says nothing either way.
                (latin == 26 && (primary == 0x09 || primary == 0)).then_some(Lang::En)
            }
            None => None,
        }
    }
}

/// Built-in tables of the three standard layouts, for tests: the real ones
/// are always read from Windows.
#[cfg(test)]
impl KeyMap {
    fn from_rows(lang: Option<Lang>, rows: &[(u8, &str, &str)]) -> Self {
        let mut chars = [[None; 2]; SCAN_CODES];
        for &(first, plain, shifted) in rows {
            let plain: Vec<char> = plain.chars().collect();
            let shifted: Vec<char> = shifted.chars().collect();
            assert_eq!(plain.len(), shifted.len());
            for (i, (&p, &s)) in plain.iter().zip(&shifted).enumerate() {
                chars[first as usize + i] = [Some(p), Some(s)];
            }
        }
        KeyMap { lang, chars }
    }

    /// US English.
    pub fn us() -> Self {
        Self::from_rows(
            Some(Lang::En),
            &[
                (0x02, "1234567890-=", "!@#$%^&*()_+"),
                (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
                (0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
                (0x2B, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
                (0x39, " ", " "),
                (0x56, "\\", "|"),
            ],
        )
    }

    /// Russian (ЙЦУКЕН).
    pub fn ru() -> Self {
        Self::from_rows(
            Some(Lang::Ru),
            &[
                (0x02, "1234567890-=", "!\"№;%:?*()_+"),
                (0x10, "йцукенгшщзхъ", "ЙЦУКЕНГШЩЗХЪ"),
                (0x1E, "фывапролджэё", "ФЫВАПРОЛДЖЭЁ"),
                (0x2B, "\\ячсмитьбю.", "/ЯЧСМИТЬБЮ,"),
                (0x39, " ", " "),
                (0x56, "\\", "/"),
            ],
        )
    }

    /// Ukrainian (Enhanced): `'` on the backtick key, `ґ` on the ISO key.
    pub fn uk() -> Self {
        Self::from_rows(
            Some(Lang::Uk),
            &[
                (0x02, "1234567890-=", "!\"№;%:?*()_+"),
                (0x10, "йцукенгшщзхї", "ЙЦУКЕНГШЩЗХЇ"),
                (0x1E, "фівапролджє'", "ФІВАПРОЛДЖЄ₴"),
                (0x2B, "\\ячсмитьбю.", "/ЯЧСМИТЬБЮ,"),
                (0x39, " ", " "),
                (0x56, "ґ", "Ґ"),
            ],
        )
    }
}

/// Reads an installed layout's table through `ToUnicodeEx`.
///
/// Flag 4 keeps the call from touching the keyboard state, so a dead key on
/// some layout cannot swallow the user's next keystroke (Windows 10 1607+).
pub fn from_hkl(hkl: windows::Win32::UI::Input::KeyboardAndMouse::HKL) -> KeyMap {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let mut chars = [[None; 2]; SCAN_CODES];
    for sc in 1..SCAN_CODES as u32 {
        let vk = unsafe { MapVirtualKeyExW(sc, MAPVK_VSC_TO_VK, hkl) };
        if vk == 0 {
            continue;
        }
        for shift in [false, true] {
            let mut state = [0u8; 256];
            if shift {
                state[VK_SHIFT.0 as usize] = 0x80;
                state[VK_LSHIFT.0 as usize] = 0x80;
            }
            let mut buf = [0u16; 8];
            let n = unsafe { ToUnicodeEx(vk, sc, &state, &mut buf, 4, hkl) };
            if n == 1
                && let Some(c) = char::from_u32(buf[0] as u32)
                && !c.is_control()
            {
                chars[sc as usize][shift as usize] = Some(c);
            }
        }
    }
    let mut map = KeyMap { lang: None, chars };
    map.lang = map.classify((hkl.0 as usize & 0xFFFF) as u16);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(sc: u8) -> Key {
        Key {
            sc,
            shift: false,
            caps: false,
        }
    }

    #[test]
    fn builtin_tables_classify_as_themselves() {
        assert_eq!(KeyMap::us().classify(0x0409), Some(Lang::En));
        assert_eq!(KeyMap::ru().classify(0x0419), Some(Lang::Ru));
        assert_eq!(KeyMap::ru().classify(0x2000), Some(Lang::Ru));
        assert_eq!(KeyMap::uk().classify(0x0422), Some(Lang::Uk));
        // A US table under a German language ID is not ours to judge.
        assert_eq!(KeyMap::us().classify(0x0407), None);
    }

    #[test]
    fn same_key_different_letters() {
        let s = key(0x1F);
        assert_eq!(KeyMap::us().render(s), Some('s'));
        assert_eq!(KeyMap::ru().render(s), Some('ы'));
        assert_eq!(KeyMap::uk().render(s), Some('і'));
        let backtick = key(0x29);
        assert_eq!(KeyMap::ru().render(backtick), Some('ё'));
        assert_eq!(KeyMap::uk().render(backtick), Some('\''));
    }

    #[test]
    fn caps_lock_only_affects_letters() {
        let caps = |sc| Key {
            sc,
            shift: false,
            caps: true,
        };
        assert_eq!(KeyMap::ru().render(caps(0x33)), Some('Б'));
        assert_eq!(KeyMap::us().render(caps(0x33)), Some(','));
        let both = Key {
            sc: 0x1E,
            shift: true,
            caps: true,
        };
        assert_eq!(KeyMap::us().render(both), Some('a'));
    }

    #[test]
    fn word_keys() {
        let us = KeyMap::us();
        let ru = KeyMap::ru();
        let uk = KeyMap::uk();
        // `,` is a letter on the Cyrillic layouts only.
        assert!(!us.is_word_key(0x33) && ru.is_word_key(0x33));
        // The apostrophe key belongs inside English words.
        assert!(us.is_word_key(0x28));
        assert!(uk.is_word_key(0x29) && uk.is_word_key(0x56));
        // `/` is punctuation everywhere.
        assert!(!us.is_word_key(0x35) && !ru.is_word_key(0x35));
    }

    #[test]
    fn key_for_inverts_render() {
        for map in [KeyMap::us(), KeyMap::ru(), KeyMap::uk()] {
            for sc in 0..SCAN_CODES as u8 {
                for shift in [false, true] {
                    let k = Key {
                        sc,
                        shift,
                        caps: false,
                    };
                    if let Some(c) = map.render(k) {
                        let back = map.key_for(c).unwrap();
                        assert_eq!(map.render(back), Some(c));
                    }
                }
            }
        }
    }
}
