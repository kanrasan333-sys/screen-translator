//! Word statistics: per language, a frequency-ranked dictionary and a
//! character trigram model, decoded from `data/punto/*.bin`.
//!
//! The blobs are built by `tools/build_punto_dicts.py` from subtitle and
//! Wikipedia word counts — roughly a quarter of a million word *forms* per
//! language, so "руку", "работаешь" and "configs" are known as themselves
//! rather than guessed at from a base form. The trigram model covers what the
//! dictionary cannot: a rare word or a typo still looks like its language,
//! while text typed on the wrong layout looks like nothing at all.
//!
//! Blob layout (zlib stream, little-endian):
//!
//! ```text
//! "PNT1"  u8 lang  u8 alphabet_len  u16 alphabet_bytes  <alphabet, UTF-8>
//! f32 oov_mass  u8 block  u32 word_count
//! K·K·K bytes: trigram table, K = alphabet_len + 1, index (a·K + b)·K + c
//! words sorted by symbol code, front-coded in blocks of `block`:
//!     block head:  u8 len, codes…, u8 q
//!     otherwise:   u8 shared_prefix, u8 suffix_len, codes…, u8 q
//! ```
//!
//! Symbol 0 is the word boundary; letters are 1..=alphabet_len in alphabet
//! order. Both `q` and the trigram entries are −log2 of a probability, in
//! eighths of a bit.

use super::keymap::Lang;
use std::sync::OnceLock;

pub struct Model {
    /// Symbol code for every code point below `LOOKUP_LEN`; 0 = not a letter
    /// of this language.
    lookup: Vec<u8>,
    k: usize,
    /// log2 of the probability mass the dictionary does not cover — the
    /// budget the trigram model shares out among unknown words.
    oov_log2: f32,
    trigrams: Vec<u8>,
    block: usize,
    count: usize,
    /// The front-coded word region.
    words: Vec<u8>,
    heads: Vec<Head>,
}

struct Head {
    offset: u32,
    /// Summed probability of every word before this block.
    mass_before: f64,
}

const LOOKUP_LEN: usize = 0x500;
const MAX_WORD: usize = 64;

/// Where a key would sit in the sorted list.
struct Seek {
    /// Summed probability of every word before that position.
    mass_before: f64,
    /// `q` of the word at that position, if it equals the key.
    exact: Option<u8>,
}

impl Model {
    pub fn load(blob: &[u8]) -> Result<Self, String> {
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(blob)
            .map_err(|e| format!("inflate: {e:?}"))?;
        let mut r = Reader { buf: &raw, pos: 0 };
        if r.bytes(4)? != b"PNT1" {
            return Err("bad magic".into());
        }
        let _lang = r.u8()?;
        let alphabet_len = r.u8()? as usize;
        let alphabet_bytes = r.u16()? as usize;
        let alphabet = std::str::from_utf8(r.bytes(alphabet_bytes)?)
            .map_err(|e| e.to_string())?
            .chars()
            .collect::<Vec<_>>();
        if alphabet.len() != alphabet_len {
            return Err("alphabet length mismatch".into());
        }
        let oov_mass = r.f32()?;
        let block = r.u8()? as usize;
        let count = r.u32()? as usize;
        let k = alphabet_len + 1;
        let trigrams = r.bytes(k * k * k)?.to_vec();
        let words = r.buf[r.pos..].to_vec();

        let mut lookup = vec![0u8; LOOKUP_LEN];
        for (i, &ch) in alphabet.iter().enumerate() {
            let cp = ch as usize;
            if cp >= LOOKUP_LEN {
                return Err(format!("letter {ch:?} outside the lookup table"));
            }
            lookup[cp] = (i + 1) as u8;
        }

        // One pass over the list: block offsets and the running mass that
        // makes prefix sums a subtraction instead of a scan.
        let mut heads = Vec::with_capacity(count / block.max(1) + 1);
        let mut pos = 0usize;
        let mut mass = 0f64;
        for i in 0..count {
            if i % block == 0 {
                heads.push(Head {
                    offset: pos as u32,
                    mass_before: mass,
                });
                let len = *words.get(pos).ok_or("truncated word list")? as usize;
                pos += 1 + len;
            } else {
                let suffix = *words.get(pos + 1).ok_or("truncated word list")? as usize;
                pos += 2 + suffix;
            }
            let q = *words.get(pos).ok_or("truncated word list")?;
            mass += prob(q);
            pos += 1;
        }
        if pos != words.len() {
            return Err("trailing bytes after the word list".into());
        }

        Ok(Model {
            lookup,
            k,
            oov_log2: oov_mass.max(1e-6).log2(),
            trigrams,
            block,
            count,
            words,
            heads,
        })
    }

    /// Symbol codes for `text`, lowercased, or `None` if any character is not
    /// a letter (or word-internal apostrophe) of this language.
    pub fn encode(&self, text: &str, out: &mut Vec<u8>) -> bool {
        out.clear();
        for ch in text.chars() {
            let ch = match ch {
                '’' | 'ʼ' | '‘' | '`' => '\'',
                c => c,
            };
            let lower = lowercase(ch);
            let code = self.lookup.get(lower as usize).copied().unwrap_or(0);
            if code == 0 {
                return false;
            }
            out.push(code);
        }
        true
    }

    /// Dictionary probability of an encoded word, if it is listed.
    pub fn word_prob(&self, codes: &[u8]) -> Option<f64> {
        self.seek(codes).exact.map(prob)
    }

    /// Summed probability of every listed word that starts with `codes`.
    pub fn prefix_mass(&self, codes: &[u8]) -> f64 {
        if codes.is_empty() {
            return 1.0;
        }
        let lo = self.seek(codes);
        let mut upper = [0u8; MAX_WORD + 1];
        let n = codes.len().min(MAX_WORD);
        upper[..n].copy_from_slice(&codes[..n]);
        // Codes never exceed 0x7F, so this sorts after every extension.
        upper[n] = 0xFF;
        let hi = self.seek(&upper[..=n]);
        (hi.mass_before - lo.mass_before).max(0.0)
    }

    /// log2 probability of `codes` under the trigram model. `whole` adds the
    /// end-of-word transition; leave it off to score a prefix.
    pub fn trigram_log2(&self, codes: &[u8], whole: bool) -> f32 {
        let k = self.k;
        let (mut a, mut b) = (0usize, 0usize);
        let mut eighths = 0u32;
        for &c in codes {
            let c = c as usize;
            eighths += self.trigrams[(a * k + b) * k + c] as u32;
            a = b;
            b = c;
        }
        if whole {
            eighths += self.trigrams[(a * k + b) * k] as u32;
        }
        -(eighths as f32) / 8.0
    }

    /// log2 probability of an encoded word: its dictionary frequency when
    /// listed, otherwise the unlisted mass shared out by the trigram model.
    pub fn word_log2(&self, codes: &[u8]) -> (f32, Option<f64>) {
        match self.word_prob(codes) {
            Some(p) => ((p as f32).log2(), Some(p)),
            None => (self.oov_log2 + self.trigram_log2(codes, true), None),
        }
    }

    fn seek(&self, key: &[u8]) -> Seek {
        if self.count == 0 {
            return Seek {
                mass_before: 0.0,
                exact: None,
            };
        }
        // Last block whose head is <= key. Heads are stored whole, so this
        // compares slices in place without decoding anything.
        let b = self
            .heads
            .partition_point(|h| self.head_word(h) <= key)
            .saturating_sub(1);

        let head = &self.heads[b];
        let mut pos = head.offset as usize;
        let mut mass = head.mass_before;
        let mut word = [0u8; 256];
        let first = b * self.block;
        let last = (first + self.block).min(self.count);
        for i in first..last {
            let len = if i == first {
                let len = self.words[pos] as usize;
                word[..len].copy_from_slice(&self.words[pos + 1..pos + 1 + len]);
                pos += 1 + len;
                len
            } else {
                let shared = self.words[pos] as usize;
                let suffix = self.words[pos + 1] as usize;
                word[shared..shared + suffix]
                    .copy_from_slice(&self.words[pos + 2..pos + 2 + suffix]);
                pos += 2 + suffix;
                shared + suffix
            };
            let q = self.words[pos];
            pos += 1;
            match word[..len].cmp(key) {
                std::cmp::Ordering::Less => mass += prob(q),
                std::cmp::Ordering::Equal => {
                    return Seek {
                        mass_before: mass,
                        exact: Some(q),
                    };
                }
                std::cmp::Ordering::Greater => {
                    return Seek {
                        mass_before: mass,
                        exact: None,
                    };
                }
            }
        }
        // Past the end of this block: the next block starts after the key.
        Seek {
            mass_before: mass,
            exact: None,
        }
    }

    fn head_word(&self, h: &Head) -> &[u8] {
        let pos = h.offset as usize;
        let len = self.words[pos] as usize;
        &self.words[pos + 1..pos + 1 + len]
    }
}

fn prob(q: u8) -> f64 {
    (-(q as f64) / 8.0).exp2()
}

/// Single-character lowercase for the scripts we handle; anything else is
/// returned unchanged and then fails the alphabet lookup.
fn lowercase(ch: char) -> char {
    let mut it = ch.to_lowercase();
    match (it.next(), it.next()) {
        (Some(c), None) => c,
        _ => ch,
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("overflow")?;
        let out = self.buf.get(self.pos..end).ok_or("truncated header")?;
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

// ============================================================
// The three languages, loaded once
// ============================================================

pub struct Models {
    pub en: Model,
    pub ru: Model,
    pub uk: Model,
}

impl Models {
    pub fn get(&self, lang: Lang) -> &Model {
        match lang {
            Lang::En => &self.en,
            Lang::Ru => &self.ru,
            Lang::Uk => &self.uk,
        }
    }
}

static MODELS: OnceLock<Option<Models>> = OnceLock::new();

/// The models if they have finished loading. Never blocks: the keyboard hook
/// calls this on every keystroke, and until the answer is `Some` it simply
/// corrects nothing.
pub fn loaded() -> Option<&'static Models> {
    MODELS.get().and_then(|m| m.as_ref())
}

/// Loads the models if nobody has yet. Takes a few dozen milliseconds, so it
/// runs on a worker at startup rather than on the first keystroke.
pub fn load() -> Option<&'static Models> {
    MODELS
        .get_or_init(|| {
            let started = std::time::Instant::now();
            let models = (|| -> Result<Models, String> {
                Ok(Models {
                    en: Model::load(include_bytes!("../../data/punto/en.bin"))?,
                    ru: Model::load(include_bytes!("../../data/punto/ru.bin"))?,
                    uk: Model::load(include_bytes!("../../data/punto/uk.bin"))?,
                })
            })();
            match models {
                Ok(m) => {
                    println!(
                        "[punto] word models loaded in {} ms",
                        started.elapsed().as_millis()
                    );
                    Some(m)
                }
                Err(e) => {
                    println!("[punto] word models failed to load: {e}");
                    None
                }
            }
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(m: &Model, w: &str) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(m.encode(w, &mut out), "{w:?} not encodable");
        out
    }

    #[test]
    fn models_load() {
        let m = load().expect("models must load");
        assert!(m.en.count > 100_000);
        assert!(m.ru.count > 100_000);
        assert!(m.uk.count > 100_000);
    }

    #[test]
    fn frequent_words_are_listed() {
        let m = load().unwrap();
        for w in ["the", "hello", "don't", "systems"] {
            assert!(m.en.word_prob(&codes(&m.en, w)).is_some(), "en {w}");
        }
        for w in ["что", "привет", "руку", "работаешь", "нибудь"] {
            assert!(m.ru.word_prob(&codes(&m.ru, w)).is_some(), "ru {w}");
        }
        for w in ["що", "привіт", "м'ясо", "купувати", "п'ять"] {
            assert!(m.uk.word_prob(&codes(&m.uk, w)).is_some(), "uk {w}");
        }
    }

    #[test]
    fn russian_is_not_passed_off_as_ukrainian() {
        // The Ukrainian subtitle corpus is a third Russian; the builder has to
        // have taken that out, or every Russian word reads as Ukrainian too.
        let m = load().unwrap();
        for w in ["что", "меня", "нет", "как", "сейчас", "его"] {
            let ru = m.ru.word_prob(&codes(&m.ru, w)).unwrap_or(0.0);
            let uk = m.uk.word_prob(&codes(&m.uk, w)).unwrap_or(0.0);
            assert!(ru > uk * 100.0, "{w}: ru {ru:e} vs uk {uk:e}");
        }
        for w in ["що", "як", "зараз", "вони", "цей"] {
            let ru = m.ru.word_prob(&codes(&m.ru, w)).unwrap_or(0.0);
            let uk = m.uk.word_prob(&codes(&m.uk, w)).unwrap_or(0.0);
            assert!(uk > ru * 100.0, "{w}: uk {uk:e} vs ru {ru:e}");
        }
    }

    #[test]
    fn letters_outside_the_alphabet_do_not_encode() {
        let m = load().unwrap();
        let mut out = Vec::new();
        assert!(!m.ru.encode("привіт", &mut out));
        assert!(!m.uk.encode("привыт", &mut out));
        assert!(!m.en.encode("hello,", &mut out));
        assert!(m.uk.encode("М’ЯСО", &mut out));
    }

    #[test]
    fn prefix_mass_counts_completions() {
        let m = load().unwrap();
        let pri = m.ru.prefix_mass(&codes(&m.ru, "прив"));
        let privet = m.ru.word_prob(&codes(&m.ru, "привет")).unwrap();
        assert!(pri > privet, "прив… must include привет");
        assert_eq!(m.ru.prefix_mass(&codes(&m.ru, "ыъщ")), 0.0);
        assert_eq!(m.en.prefix_mass(&codes(&m.en, "ghbd")), 0.0);
        let total = m.en.prefix_mass(&[]);
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn lookups_agree_with_a_linear_scan() {
        // Block boundaries are where a front-coded search goes wrong, so
        // check every position of a few blocks against brute force.
        let m = load().unwrap();
        let model = &m.ru;
        let mut all = Vec::new();
        let mut pos = 0usize;
        let mut prev: Vec<u8> = Vec::new();
        for i in 0..model.count.min(3000) {
            let word: Vec<u8> = if i % model.block == 0 {
                let len = model.words[pos] as usize;
                let w = model.words[pos + 1..pos + 1 + len].to_vec();
                pos += 1 + len;
                w
            } else {
                let shared = model.words[pos] as usize;
                let suffix = model.words[pos + 1] as usize;
                let mut w = prev[..shared].to_vec();
                w.extend_from_slice(&model.words[pos + 2..pos + 2 + suffix]);
                pos += 2 + suffix;
                w
            };
            let q = model.words[pos];
            pos += 1;
            assert!(word > prev || i == 0, "list must be sorted");
            prev = word.clone();
            all.push((word, q));
        }
        for (w, q) in &all {
            assert_eq!(model.word_prob(w), Some(prob(*q)));
        }
    }

    #[test]
    fn trigrams_tell_words_from_layout_noise() {
        let m = load().unwrap();
        let per = |model: &Model, w: &str| {
            let c = codes(model, w);
            model.trigram_log2(&c, true) / (c.len() + 1) as f32
        };
        // A typo is still Russian; the same keys on the wrong layout are not.
        assert!(per(&m.ru, "кучча") > per(&m.en, "rexxf"));
        assert!(per(&m.en, "isometcir") > per(&m.ru, "шыщьуесшк"));
        assert!(per(&m.ru, "привет") > per(&m.en, "ghbdtn"));
    }
}
