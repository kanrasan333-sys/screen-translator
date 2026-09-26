//! Whether a word was typed on the wrong layout, and which one was meant.
//!
//! Every decision compares *readings*: the same keystrokes rendered through
//! each installed layout, scored as text in that layout's language. A reading
//! scores the log-probability of the words in it — dictionary frequency when
//! the word is listed, the trigram model's estimate when it is not — so a real
//! word, a rare word, a typo and layout noise all land on one scale and the
//! question becomes how much likelier one reading is than the other.
//!
//! The text as typed gets a head start, sized by how much it looks like
//! something meant. A common word ("руку", "tv") keeps it whatever the other
//! reading says; a rare one ("еру", the Ukrainian accusative of "era", which is
//! also "the" on a Russian layout) loses to a far likelier reading; noise loses
//! to any real word. And the replacement always has to be a word in its own
//! right, not merely less unlikely than what was typed.
//!
//! Russian and Ukrainian share most of their spelling, so between the two the
//! words alone often cannot decide: "стороны" and "стороні" are the same keys
//! and both correct. There the language the user has been writing in does.

use super::keymap::{Key, KeyMap, Lang};
use super::model::{Model, Models};

/// A layout the user could be moved to: one per installed language.
pub struct Target<'a> {
    pub lang: Lang,
    pub map: &'a KeyMap,
}

pub struct Context<'a> {
    pub models: &'a Models,
    pub targets: &'a [Target<'a>],
    /// Recent Cyrillic usage: positive leans Ukrainian, negative Russian.
    /// Weighs only between the two Cyrillic languages, never against English.
    pub bias: f32,
    /// The word just before this one was typed on the same layout and left
    /// as it was: the text is running on in this language.
    pub continues: bool,
    /// Words the user has sent back with the undo key.
    pub rejected: &'a dyn Fn(&str) -> bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// Retype the word as `text`, on `targets[target]`.
    Convert { target: usize, text: String },
    /// Leave the text; it is right, but the layout is the wrong Cyrillic one —
    /// "купувати" typed on a Russian layout, one keystroke before the first
    /// `і` comes out as `ы`.
    Relayout { target: usize },
}

// ============================================================
// Tuning
// ============================================================
//
// All in bits (log2), set against the held-out word lists of
// tools/build_punto_dicts.py by the `eval` test.

/// Head start of a typed text that is a listed word, by its frequency.
///
/// A word as common as "руку" (2^-13.6) or "tv" keeps the full 14 bits, which
/// no pair of real words spans ("руку" vs "here" is 2^5). Every halving below
/// that costs 1.5 bits, down to 7: "еру" at 2^-18 then yields to "the", which
/// is 2^13.8 likelier.
fn word_margin(log2: f32) -> f32 {
    (14.0 + 1.5 * (log2 + 13.3)).clamp(7.0, 14.0)
}

/// Head start of a typed text that is not a listed word, by how much it still
/// looks like one. A typo or a rare name keeps the shape of its language
/// ("кучча"), layout noise does not ("руддщ"); three-letter strings are cheap
/// to hit by accident in either reading, so they get a little more.
///
/// Two letters have no shape to speak of — "ше" is as good a syllable as
/// any — so they get a flat head start, which common words on the other side
/// clear ("it", "мы") and noise does not.
fn oov_margin(shape: f32, letters: usize) -> f32 {
    if letters <= 2 {
        return 6.0;
    }
    let wordlike = ((shape - SHAPE_NOISE) / (SHAPE_WORDLIKE - SHAPE_NOISE)).clamp(0.0, 1.0);
    3.5 + 6.5 * wordlike + if letters == 3 { 2.0 } else { 0.0 }
}

/// When the word before was left on this layout, the sentence is running on
/// in this language: "React vs Vue" keeps its "vs", which alone would read as
/// a Russian "мы" typed on the wrong layout.
const CONTINUES_BITS: f32 = 4.0;

/// After a mid-word fix the screen shows our guess, not what was typed, so
/// neither reading deserves a head start — only enough to stop flip-flopping.
const NEUTRAL_MARGIN: f32 = 2.0;

/// How frequent a listed word must be, by length, to count as a real word.
/// The corpora hold plenty of two-letter noise — "nj" (New Jersey) sits at
/// 2.5e-6 — that must not stand in for "то".
fn min_word_prob(letters: usize) -> f64 {
    match letters {
        0..=1 => 1e-3,
        2 => 2e-5,
        3 => 2e-6,
        4 => 2e-7,
        _ => 2e-8,
    }
}

/// Trigram bits per transition. Real words sit around -3 to -4.5; the
/// Cyrillic of Latin keys typed on a Russian layout, and vice versa, below -6.
const SHAPE_WORDLIKE: f32 = -4.6;
const SHAPE_NOISE: f32 = -6.0;

/// Punctuation inside a reading. Where text normally has it — a quote before
/// an English word, a period or comma after one — a mark costs a little.
/// Anywhere else ("," glued to the front of a word, `[` after one, and any
/// mark at all in Cyrillic, whose layouts put letters on those keys) it
/// costs a lot, and the reading loses most of its standing as typed text.
const PUNCT_COST: f32 = 3.0;
const ODD_PUNCT_COST: f32 = 8.0;
const PART_COST: f32 = 4.0;

/// Head start of a reading with punctuation where no text would have it.
const UNCLEAN_MARGIN: f32 = 4.0;

/// Punctuation English puts right after a word.
fn trails_words(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '\'' | '"' | ']' | '}' | '?' | '!' | ')')
}

/// How far recent usage may tilt a Russian/Ukrainian choice: enough to pick
/// the layout for a spelling both languages share ("так", "про"), not enough
/// to pass a Russian word off as Ukrainian ("что" is 2^12 likelier in
/// Russian).
const BIAS_BITS: f32 = 6.0;

/// ...and how far it may tilt *whether* a real word typed on one Cyrillic
/// layout is the other language's word ("стороны" or "стороні"). Much less:
/// the layout the user is on says something too, and when the context is
/// stale — Russian typed right after a Ukrainian conversation — a full tilt
/// would rewrite correct words wholesale.
const KEEP_TILT_BITS: f32 = 2.0;

/// A text of punctuation marks only is replaced by nothing rarer than this.
const PUNCT_ONLY_MIN_LOG2: f32 = -13.3;

/// Between the two Cyrillic layouts a typed text that is no word at all has
/// less to protect: the other reading differs only in the letters the two
/// alphabets do not share, and "привыт" or "мый" is Ukrainian typed on the
/// Russian layout far more often than a Russian word nobody has heard of.
const SIBLING_DISCOUNT: f32 = 4.0;

/// Relayout needs a little more than a conversion: nothing on screen changes,
/// so a wrong one is noticed only several words later.
const RELAYOUT_EXTRA: f32 = 2.0;

/// ...and the word has to be a known one in the other language, not a stray
/// from the far end of its list. Each dictionary's tail holds a scattering of
/// the other language ("ампутировал" among the Ukrainian ones), and a rare
/// Russian word must not send the user to the Ukrainian layout because of it.
const RELAYOUT_MIN_LOG2: f32 = -21.5;

/// The next word was just corrected from this same layout: the one before it
/// was most likely typed on the wrong layout too.
const RETRO_BITS: f32 = 8.0;

/// A single key is converted only between the two Cyrillic layouts, and only
/// by this much: `ы` alone is no Russian word, `і` is the commonest Ukrainian
/// one, and they share a key. Against English a lone letter is anybody's.
const SINGLE_MARGIN: f32 = 8.0;

/// Mid-word window, in keystrokes. Below it a dead prefix is weak evidence;
/// above it the word is nearly done and the boundary check — which sees the
/// whole word — is a moment away.
pub const PARTIAL_MIN: usize = 4;
pub const PARTIAL_MAX: usize = 7;

/// Mid-word: the typed prefix must look like noise (bits per keystroke), and
/// the other reading must look like a word and lead into real ones.
const PARTIAL_TYPED_SHAPE: f32 = -5.5;
const PARTIAL_TARGET_SHAPE: f32 = -4.8;
const PARTIAL_MIN_MASS: f64 = 1e-6;

// ============================================================
// Scoring a reading
// ============================================================

#[derive(Debug, Clone, Copy)]
struct Score {
    lang: Lang,
    log2: f32,
    /// Every word in the reading is in the dictionary.
    listed: bool,
    /// ...and frequent enough for its length to count as a real word.
    word: bool,
    /// Reads as a word with punctuation around it, the way text is written:
    /// nothing glued to its front but a quote, nothing inside but an
    /// apostrophe (and, in English, the dot of "github.com"). ",s" is not.
    clean: bool,
    letters: usize,
    /// Trigram bits per transition of the reading's longest word.
    shape: f32,
}

impl Score {
    /// Head start this reading gets as the text the user typed. A listed
    /// token is judged by its frequency, however rare — "tot" is a real if
    /// uncommon string, and 2^-20 of it does not stand up to "еще"; only an
    /// unlisted one falls back on how word-like it looks.
    fn margin(&self) -> f32 {
        if !self.clean {
            UNCLEAN_MARGIN
        } else if self.listed {
            word_margin(self.log2)
        } else {
            oov_margin(self.shape, self.letters)
        }
    }
}

/// Splits `text` into words of `lang` and scores them.
///
/// An apostrophe between two letters stays inside the word ("don't",
/// "м'ясо"); punctuation costs a little and may split the text into several
/// words ("github.com" is two). `None` when there are no letters, or when a
/// letter is not of this language at all — `ы` makes a reading not Ukrainian,
/// it does not make it Ukrainian with a punctuation mark.
fn score(models: &Models, lang: Lang, text: &str) -> Option<Score> {
    let model = models.get(lang);
    let chars: Vec<char> = text.chars().collect();
    let mut probe = Vec::new();
    let mut is_letter = |c: char| c != '\'' && model.encode(c.encode_utf8(&mut [0; 4]), &mut probe);
    let apostrophe_ok = apostrophe_code(model) != 0;

    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut penalty = 0.0f32;
    let mut clean = true;
    for (i, &c) in chars.iter().enumerate() {
        if is_letter(c) {
            current.push(c);
            continue;
        }
        let inside = !current.is_empty() && chars.get(i + 1).is_some_and(|&n| is_letter(n));
        if matches!(c, '\'' | '’' | 'ʼ') && apostrophe_ok && inside {
            current.push('\'');
            continue;
        }
        if c.is_alphabetic() {
            return None;
        }
        let leading = parts.is_empty() && current.is_empty();
        let joins = !leading && chars[i + 1..].iter().any(|&n| is_letter(n));
        let natural = lang == Lang::En
            && if leading {
                matches!(c, '"' | '\'')
            } else if joins {
                // "github.com", "readme.md"
                c == '.'
            } else {
                trails_words(c)
            };
        if natural {
            penalty += PUNCT_COST;
        } else {
            penalty += ODD_PUNCT_COST;
            clean = false;
        }
        if !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        return None;
    }

    let mut buf = Vec::new();
    let mut log2 = -penalty - (parts.len() - 1) as f32 * PART_COST;
    let mut listed = true;
    let mut word = true;
    let mut letters = 0;
    let mut shape = 0.0;
    let mut longest = 0;
    for part in &parts {
        if !model.encode(part, &mut buf) {
            return None;
        }
        let (lp, p) = model.word_log2(&buf);
        log2 += lp;
        listed &= p.is_some();
        word &= p.is_some_and(|p| p >= min_word_prob(buf.len()));
        letters += buf.len();
        if buf.len() > longest {
            longest = buf.len();
            shape = model.trigram_log2(&buf, true) / (buf.len() + 1) as f32;
        }
    }
    Some(Score {
        lang,
        log2,
        listed,
        word,
        clean,
        letters,
        shape,
    })
}

/// The other Cyrillic language, which can spell most of the same text.
fn sibling(lang: Lang) -> Option<Lang> {
    match lang {
        Lang::Ru => Some(Lang::Uk),
        Lang::Uk => Some(Lang::Ru),
        Lang::En => None,
    }
}

/// How the text as typed scores: as its layout's language, and as the sibling
/// Cyrillic one when it is spelled the same there. "оце" typed on a Russian
/// layout is not Russian, but it is a perfectly good Ukrainian word, and that
/// is reason enough not to turn it into English.
fn typed_scores(models: &Models, lang: Lang, text: &str) -> (Option<Score>, Option<Score>) {
    let own = score(models, lang, text);
    let other = sibling(lang).and_then(|s| score(models, s, text));
    (own, other)
}

fn bias_bits(lang: Lang, bias: f32) -> f32 {
    match lang {
        Lang::Uk => BIAS_BITS * bias,
        Lang::Ru => -BIAS_BITS * bias,
        Lang::En => 0.0,
    }
}

/// Latin that is code or shorthand rather than a word: camelCase
/// ("getElementById", "iPhone") or a short run of capitals ("WS", "FFFF",
/// "KB" — constants and acronyms). Left alone, whatever the other layouts
/// make of it.
///
/// Only Latin: Cyrillic is never written that way, so "шЗрщту" is an
/// "iPhone" typed on the wrong layout and gets no such protection. (Nor is
/// "a.b" a sign of code: "c.lf" is "сюда", the `.` key being `ю`.)
fn identifier(text: &str, lang: Lang) -> bool {
    if lang != Lang::En {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let camel = chars
        .windows(2)
        .any(|w| w[0].is_lowercase() && w[1].is_uppercase());
    let letters: Vec<char> = chars.iter().copied().filter(|c| c.is_alphabetic()).collect();
    let acronym = (2..=4).contains(&letters.len()) && letters.iter().all(|c| c.is_uppercase());
    camel || acronym
}

/// A converted reading good enough to replace the text: a real word, or a
/// long unknown one that looks like its language while the typed text looks
/// like nothing at all.
fn confirmed(s: &Score, kept_shape: f32) -> bool {
    s.letters >= 2
        && s.clean
        && (s.word || (s.letters >= 6 && s.shape >= SHAPE_WORDLIKE && kept_shape <= SHAPE_NOISE))
}

// ============================================================
// Decisions
// ============================================================

/// Decides a finished word.
///
/// `shown`/`lang` is the layout the keys are on screen in. `neutral` is set
/// after a mid-word fix: the screen then shows our guess rather than what the
/// user typed, so the typed reading gets no head start.
pub fn boundary(ctx: &Context, keys: &[Key], shown: &KeyMap, lang: Lang, neutral: bool) -> Verdict {
    let Some(typed) = shown.render_all(keys) else {
        return Verdict::Keep;
    };
    if keys.is_empty() || identifier(&typed, lang) || (ctx.rejected)(&typed.to_lowercase()) {
        return Verdict::Keep;
    }
    if keys.len() == 1 {
        return single(ctx, keys, &typed, lang);
    }

    // What a replacement has to beat: the text as typed, read in its own
    // language or the sibling one, and the head start it gets.
    let (own, sib) = typed_scores(ctx.models, lang, &typed);
    let kept: Option<(Score, f32)> = [own, sib]
        .into_iter()
        .flatten()
        .map(|s| {
            let head = if neutral {
                NEUTRAL_MARGIN
            } else {
                s.margin() + if ctx.continues { CONTINUES_BITS } else { 0.0 }
            };
            (s, head)
        })
        .max_by(|a, b| (a.0.log2 + a.1).total_cmp(&(b.0.log2 + b.1)));
    let kept_shape = kept.map_or(f32::NEG_INFINITY, |(s, _)| s.shape);
    // The bar for a reading in `to`. Context weighs in only between two real
    // words of the two Cyrillic languages — "стороны" and "стороні" — never
    // for a typed text that is no word at all.
    let bar = |to: Lang| -> (f32, f32) {
        let Some((s, head)) = kept else {
            // Not a letter in it: "};" and "]]" are mostly punctuation on
            // purpose, and only a common word ("її", "їх") is reason enough
            // to read them as letters.
            return (PUNCT_ONLY_MIN_LOG2, 0.0);
        };
        let siblings = s.lang.is_cyrillic() && to.is_cyrillic();
        if !siblings {
            return (s.log2 + head, 0.0);
        }
        if s.listed {
            let tilt = (bias_bits(to, ctx.bias) - bias_bits(s.lang, ctx.bias)) * KEEP_TILT_BITS / BIAS_BITS;
            (s.log2 + head, tilt)
        } else if neutral {
            (s.log2 + head, 0.0)
        } else {
            (s.log2 + (head - SIBLING_DISCOUNT).max(NEUTRAL_MARGIN), 0.0)
        }
    };

    struct Cand {
        target: usize,
        text: String,
        score: Score,
        clears: bool,
    }
    let mut cands: Vec<Cand> = Vec::new();
    for (i, t) in ctx.targets.iter().enumerate() {
        if t.lang == lang {
            continue;
        }
        let Some(text) = t.map.render_all(keys) else {
            continue;
        };
        if text == typed {
            continue;
        }
        let Some(s) = score(ctx.models, t.lang, &text) else {
            continue;
        };
        if !confirmed(&s, kept_shape) {
            continue;
        }
        let (bar, tilt) = bar(t.lang);
        let clears = s.log2 + tilt >= bar;
        cands.push(Cand {
            target: i,
            text,
            score: s,
            clears,
        });
    }
    // Both Cyrillic layouts often turn Latin keys into the same text; then
    // whether to convert is one question and which layout to land on another.
    // Either reading clearing the bar settles the first, the context the second.
    let cleared: Vec<String> = cands.iter().filter(|c| c.clears).map(|c| c.text.clone()).collect();
    if let Some(best) = cands
        .into_iter()
        .filter(|c| cleared.contains(&c.text))
        .max_by(|a, b| {
            let ra = a.score.log2 + bias_bits(a.score.lang, ctx.bias);
            let rb = b.score.log2 + bias_bits(b.score.lang, ctx.bias);
            ra.total_cmp(&rb)
        })
    {
        return Verdict::Convert {
            target: best.target,
            text: best.text,
        };
    }

    // Right text, wrong Cyrillic layout. The words alone decide this one:
    // context has no say in switching a layout under text that is correct.
    if let (Some(own), Some(sib), Some(other)) = (own, sib, sibling(lang))
        && sib.word
        && sib.clean
        && sib.log2 >= RELAYOUT_MIN_LOG2
        && sib.log2 >= own.log2 + own.margin() + RELAYOUT_EXTRA
        && let Some(target) = ctx.targets.iter().position(|t| t.lang == other)
    {
        return Verdict::Relayout { target };
    }
    Verdict::Keep
}

/// A word of one key: only ever moved between the two Cyrillic layouts.
fn single(ctx: &Context, keys: &[Key], typed: &str, lang: Lang) -> Verdict {
    let Some(other) = sibling(lang) else {
        return Verdict::Keep;
    };
    let Some(target) = ctx.targets.iter().position(|t| t.lang == other) else {
        return Verdict::Keep;
    };
    let Some(text) = ctx.targets[target].map.render_all(keys) else {
        return Verdict::Keep;
    };
    if text == typed {
        return Verdict::Keep;
    }
    let Some(cand) = score(ctx.models, other, &text).filter(|s| s.word) else {
        return Verdict::Keep;
    };
    let own = score(ctx.models, lang, typed).map_or(-40.0, |s| s.log2);
    if cand.log2 + bias_bits(other, ctx.bias) >= own + bias_bits(lang, ctx.bias) + SINGLE_MARGIN {
        Verdict::Convert { target, text }
    } else {
        Verdict::Keep
    }
}

/// Re-decides the word before one we just corrected from the same layout.
///
/// Left alone on its own, a short word is usually ambiguous — "ye" is
/// English, if archaic, and ",s" could be anything — but the next word
/// settled which layout the user thought they were on. So the word before it
/// is judged again with that behind it, and follows along when it reads as a
/// real word there: "ye ghbdtn" becomes "ну привет", not "ye привет".
pub fn retro(ctx: &Context, keys: &[Key], shown: &KeyMap, lang: Lang, to: usize) -> Option<String> {
    let t = ctx.targets.get(to)?;
    let typed = shown.render_all(keys)?;
    if identifier(&typed, lang) || (ctx.rejected)(&typed.to_lowercase()) {
        return None;
    }
    let text = t.map.render_all(keys)?;
    if text == typed {
        return None;
    }
    let s = score(ctx.models, t.lang, &text)?;
    // Single letters count here — "z" before a Russian word is "я".
    let real = s.clean && (s.word || (s.letters == 1 && s.log2 >= (min_word_prob(1) as f32).log2()));
    if !real {
        return None;
    }
    let (own, sib) = typed_scores(ctx.models, lang, &typed);
    let bar = [own, sib]
        .into_iter()
        .flatten()
        .map(|k| k.log2 + k.margin())
        .fold(f32::NEG_INFINITY, f32::max);
    (s.log2 + RETRO_BITS >= bar).then_some(text)
}

/// Decides a word still being typed: `Some` only when the prefix already
/// leads nowhere in its own language and clearly somewhere in another.
pub fn partial(ctx: &Context, keys: &[Key], shown: &KeyMap, lang: Lang) -> Option<(usize, String)> {
    if !(PARTIAL_MIN..=PARTIAL_MAX).contains(&keys.len()) {
        return None;
    }
    let typed = shown.render_all(keys)?;
    if identifier(&typed, lang) || (ctx.rejected)(&typed.to_lowercase()) {
        return None;
    }
    let model = ctx.models.get(lang);
    let mut buf = Vec::new();
    if !model.encode(&typed, &mut buf) || buf.contains(&apostrophe_code(model)) {
        return None;
    }
    // Alive in its own language, or in the sibling one (a Ukrainian word on a
    // Russian layout is not a mistake yet): wait for the whole word.
    if model.prefix_mass(&buf) > 0.0 {
        return None;
    }
    let typed_shape = model.trigram_log2(&buf, false) / buf.len() as f32;
    if typed_shape > PARTIAL_TYPED_SHAPE {
        return None;
    }
    if let Some(s) = sibling(lang) {
        let sm = ctx.models.get(s);
        let mut sb = Vec::new();
        if sm.encode(&typed, &mut sb) && sm.prefix_mass(&sb) > 0.0 {
            return None;
        }
    }

    let mut best: Option<(f32, usize, String)> = None;
    for (i, t) in ctx.targets.iter().enumerate() {
        if t.lang == lang {
            continue;
        }
        let Some(text) = t.map.render_all(keys) else {
            continue;
        };
        let m = ctx.models.get(t.lang);
        let mut tb = Vec::new();
        if text == typed || !m.encode(&text, &mut tb) || tb.contains(&apostrophe_code(m)) {
            continue;
        }
        let mass = m.prefix_mass(&tb);
        let shape = m.trigram_log2(&tb, false) / tb.len() as f32;
        if mass < PARTIAL_MIN_MASS || shape < PARTIAL_TARGET_SHAPE {
            continue;
        }
        let rank = mass.log2() as f32 + bias_bits(t.lang, ctx.bias);
        if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
            best = Some((rank, i, text));
        }
    }
    best.map(|(_, i, text)| (i, text))
}

/// The undo key pressed on a word we did not touch: the user says it is on
/// the wrong layout, so it goes to the likeliest other one, however unsure.
pub fn forced(ctx: &Context, keys: &[Key], lang: Lang) -> Option<(usize, String)> {
    let mut best: Option<(f32, usize, String)> = None;
    for (i, t) in ctx.targets.iter().enumerate() {
        if t.lang == lang {
            continue;
        }
        let Some(text) = t.map.render_all(keys) else {
            continue;
        };
        let rank = score(ctx.models, t.lang, &text)
            .map_or(-200.0, |s| s.log2 + bias_bits(t.lang, ctx.bias));
        if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
            best = Some((rank, i, text));
        }
    }
    best.map(|(_, i, text)| (i, text))
}

/// Retypes a selection that was typed on the wrong layout.
///
/// Recovers the keystrokes from the text (on whichever installed layout can
/// type every letter of it), then renders them through every other layout and
/// keeps the reading whose words score best. Whitespace and digits are the
/// same everywhere and are carried over as they are. Returns the new text and
/// the target's index.
pub fn convert_text(ctx: &Context, text: &str) -> Option<(String, usize)> {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    // The source: a layout that can type every letter. Both Cyrillic layouts
    // put their shared letters on the same keys, so for text either could
    // have typed the choice does not matter.
    let source = ctx
        .targets
        .iter()
        .position(|t| letters.iter().all(|&c| t.map.key_for(c).is_some()))?;
    let src = &ctx.targets[source];
    let keys: Vec<Option<Key>> = text
        .chars()
        .map(|c| {
            if c.is_whitespace() || c.is_ascii_digit() {
                None
            } else {
                src.map.key_for(c)
            }
        })
        .collect();

    let mut best: Option<(f32, usize, String)> = None;
    for (i, t) in ctx.targets.iter().enumerate() {
        if t.lang == src.lang {
            continue;
        }
        let out: String = text
            .chars()
            .zip(&keys)
            .map(|(c, k)| k.and_then(|k| t.map.render(k)).unwrap_or(c))
            .collect();
        let rank: f32 = out
            .split(char::is_whitespace)
            .filter(|w| !w.is_empty())
            .map(|w| score(ctx.models, t.lang, w).map_or(-60.0, |s| s.log2))
            .sum::<f32>()
            + bias_bits(t.lang, ctx.bias);
        if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
            best = Some((rank, i, out));
        }
    }
    best.map(|(_, i, out)| (out, i))
}

fn apostrophe_code(model: &Model) -> u8 {
    let mut b = Vec::new();
    if model.encode("'", &mut b) { b[0] } else { 0 }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::autotype::model;

    pub struct Fixture {
        pub us: KeyMap,
        pub ru: KeyMap,
        pub uk: KeyMap,
    }

    impl Fixture {
        pub fn new() -> Self {
            Fixture {
                us: KeyMap::us(),
                ru: KeyMap::ru(),
                uk: KeyMap::uk(),
            }
        }
        pub fn map(&self, lang: Lang) -> &KeyMap {
            match lang {
                Lang::En => &self.us,
                Lang::Ru => &self.ru,
                Lang::Uk => &self.uk,
            }
        }
        pub fn targets(&self) -> Vec<Target<'_>> {
            Lang::ALL
                .into_iter()
                .map(|lang| Target {
                    lang,
                    map: self.map(lang),
                })
                .collect()
        }
    }

    /// Keys that type `word` on `lang`'s layout.
    pub fn keys_of(fx: &Fixture, lang: Lang, word: &str) -> Option<Vec<Key>> {
        word.chars().map(|c| fx.map(lang).key_for(c)).collect()
    }

    pub fn never(_: &str) -> bool {
        false
    }

    /// A user who has been writing `lang` for a while.
    pub fn bias_for(lang: Lang) -> f32 {
        match lang {
            Lang::Ru => -0.7,
            Lang::Uk => 0.7,
            Lang::En => 0.0,
        }
    }

    /// The verdict on `word`, written for `intended` but typed on `on`.
    fn verdict(intended: Lang, word: &str, on: Lang) -> (Verdict, Vec<Lang>) {
        let fx = Fixture::new();
        let targets = fx.targets();
        let langs = targets.iter().map(|t| t.lang).collect();
        let ctx = Context {
            models: model::load().unwrap(),
            targets: &targets,
            bias: bias_for(intended),
            continues: false,
            rejected: &never,
        };
        let keys = keys_of(&fx, intended, word).expect("typeable");
        (boundary(&ctx, &keys, fx.map(on), on, false), langs)
    }

    #[track_caller]
    fn fixes(intended: Lang, word: &str, on: Lang) {
        let (v, langs) = verdict(intended, word, on);
        match v {
            Verdict::Convert { target, ref text } if langs[target] == intended && text == word => {}
            other => panic!("{word:?} ({intended:?}) typed on {on:?}: got {other:?}"),
        }
    }

    #[track_caller]
    fn keeps(lang: Lang, word: &str) {
        let (v, _) = verdict(lang, word, lang);
        assert_eq!(v, Verdict::Keep, "{word:?} typed correctly on {lang:?}");
    }

    #[test]
    fn fixes_the_words_from_the_field_log() {
        // Every one of these was mishandled by the previous implementation.
        fixes(Lang::Ru, "большими", Lang::En); // `,` is б — used to split the word
        fixes(Lang::Ru, "бесконечную", Lang::En); // б and ю both
        fixes(Lang::Ru, "до", Lang::En);
        fixes(Lang::Ru, "то", Lang::En);
        fixes(Lang::Ru, "про", Lang::En);
        fixes(Lang::Ru, "тот", Lang::En);
        fixes(Lang::En, "it", Lang::Ru);
        fixes(Lang::En, "app", Lang::Ru);
        fixes(Lang::Uk, "купувати", Lang::En);
        fixes(Lang::En, "hello", Lang::Ru);
        fixes(Lang::En, "the", Lang::Ru);
        fixes(Lang::Ru, "привет", Lang::En);
        fixes(Lang::Uk, "привіт", Lang::En);
        fixes(Lang::Ru, "еще", Lang::En);
        fixes(Lang::Ru, "тут", Lang::En);
        fixes(Lang::Uk, "всі", Lang::En);
    }

    #[test]
    fn keeps_real_words_whose_other_reading_is_also_a_word() {
        keeps(Lang::Ru, "руку"); // "here"
        keeps(Lang::Ru, "ем"); // "tv"
        keeps(Lang::En, "tv");
        keeps(Lang::Uk, "оце"); // "jwt"
        keeps(Lang::Uk, "ті"); // "ты"
        keeps(Lang::Ru, "кучча"); // a typo, not English "rexxf"
        keeps(Lang::Ru, "афе"); // a fragment, not "fat"
        keeps(Lang::Ru, "рук"); // "her"
    }

    #[test]
    fn a_ukrainian_word_on_a_russian_layout_is_not_turned_english() {
        // "оце" on a Russian layout: not Russian, but Ukrainian, and the
        // English reading "jwt" is no reason to touch it.
        let (v, _) = verdict(Lang::Uk, "оце", Lang::Ru);
        assert!(!matches!(v, Verdict::Convert { .. }), "{v:?}");
    }

    #[test]
    fn russian_and_ukrainian_are_told_apart_by_the_word() {
        fixes(Lang::Uk, "привіт", Lang::Ru); // і typed as ы
        fixes(Lang::Uk, "ні", Lang::Ru);
        fixes(Lang::Uk, "мені", Lang::Ru);
        fixes(Lang::Uk, "має", Lang::Ru);
        fixes(Lang::Uk, "мій", Lang::Ru);
        fixes(Lang::Ru, "мы", Lang::Uk); // ы typed as і
        fixes(Lang::Ru, "это", Lang::Uk);
        // "біла" is a word too; alone it stays — see the engine's
        // `ambiguous_word_follows_the_sentence`.
        keeps(Lang::Uk, "біла");
        fixes(Lang::Uk, "підпис", Lang::Ru);
        // Same letters either way: the word itself decides, against the bias.
        let fx = Fixture::new();
        let targets = fx.targets();
        let pick = |word: &str, intended: Lang, bias: f32| {
            let ctx = Context {
                models: model::load().unwrap(),
                targets: &targets,
                bias,
                continues: false,
                rejected: &never,
            };
            let keys = keys_of(&fx, intended, word).unwrap();
            match boundary(&ctx, &keys, &fx.us, Lang::En, false) {
                Verdict::Convert { target, .. } => Some(targets[target].lang),
                _ => None,
            }
        };
        assert_eq!(pick("сейчас", Lang::Ru, 0.8), Some(Lang::Ru));
        assert_eq!(pick("зараз", Lang::Uk, -0.8), Some(Lang::Uk));
        // Shared words follow the user's recent language.
        assert_eq!(pick("так", Lang::Ru, -0.7), Some(Lang::Ru));
        assert_eq!(pick("так", Lang::Uk, 0.7), Some(Lang::Uk));
        assert_eq!(pick("ну", Lang::Uk, 0.7), Some(Lang::Uk));
    }

    #[test]
    fn relayout_when_only_the_layout_is_wrong() {
        let (v, langs) = verdict(Lang::Uk, "купувати", Lang::Ru);
        assert!(matches!(v, Verdict::Relayout { target } if langs[target] == Lang::Uk), "{v:?}");
        let (v, langs) = verdict(Lang::Ru, "сейчас", Lang::Uk);
        assert!(matches!(v, Verdict::Relayout { target } if langs[target] == Lang::Ru), "{v:?}");
        let (v, langs) = verdict(Lang::Ru, "что", Lang::Uk);
        assert!(matches!(v, Verdict::Relayout { target } if langs[target] == Lang::Ru), "{v:?}");
        // Words both languages share are left where they are.
        for w in ["тебе", "так", "мама"] {
            let (v, _) = verdict(Lang::Uk, w, Lang::Ru);
            assert_eq!(v, Verdict::Keep, "{w}");
        }
    }

    #[test]
    fn punctuation_rides_along() {
        // English typed on a Russian layout, ending in a comma or a period:
        // those keys are б and ю there.
        fixes(Lang::En, "hello,", Lang::Ru);
        fixes(Lang::En, "done.", Lang::Ru);
        fixes(Lang::En, "don't", Lang::Ru);
        fixes(Lang::Uk, "м'ясо", Lang::En);
        fixes(Lang::Uk, "її", Lang::En); // "]]": letters, not brackets
        fixes(Lang::Uk, "їх", Lang::En);
        fixes(Lang::Ru, "сюда", Lang::En); // "c.lf": not a field access
        keeps(Lang::En, "};");
        fixes(Lang::Ru, "бы", Lang::En); // ",s" — a comma glued to a letter is no English
        keeps(Lang::En, "hello,");
        keeps(Lang::En, "e.g.");
        // A Ukrainian reading cannot start with the apostrophe key's `'`.
        keeps(Lang::Ru, "ёта");
    }

    #[test]
    fn identifiers_are_left_alone() {
        for w in ["iPhone", "getElementById", "json", "nginx", "ffmpeg", "npm", "gta"] {
            keeps(Lang::En, w);
        }
        for w in ["json", "nginx", "ffmpeg", "github", "gta", "iPhone"] {
            fixes(Lang::En, w, Lang::Ru);
        }
    }

    #[test]
    fn single_keys() {
        // Against English a lone letter is anybody's.
        for (w, lang) in [("c", Lang::En), ("b", Lang::En), ("s", Lang::En), ("в", Lang::Ru)] {
            keeps(lang, w);
        }
        // Between the Cyrillic layouts `ы` alone is no word, `і` the commonest.
        fixes(Lang::Uk, "і", Lang::Ru);
        fixes(Lang::Uk, "є", Lang::Ru);
        keeps(Lang::Ru, "ы");
    }

    #[test]
    fn the_word_before_follows_a_correction() {
        let fx = Fixture::new();
        let targets = fx.targets();
        let ctx = Context {
            models: model::load().unwrap(),
            targets: &targets,
            bias: -0.7,
            continues: false,
            rejected: &never,
        };
        let ru = 1;
        let retro = |w: &str| {
            let keys = keys_of(&fx, Lang::Ru, w).unwrap();
            super::retro(&ctx, &keys, &fx.us, Lang::En, ru)
        };
        assert_eq!(retro("ну").as_deref(), Some("ну")); // "ye"
        assert_eq!(retro("бы").as_deref(), Some("бы")); // ",s"
        assert_eq!(retro("я").as_deref(), Some("я")); // "z"
        assert_eq!(retro("мы").as_deref(), Some("мы")); // "vs"
        // A real English word stays English.
        for w in ["the", "a", "I"] {
            let keys = keys_of(&fx, Lang::En, w).unwrap();
            assert_eq!(super::retro(&ctx, &keys, &fx.us, Lang::En, ru), None, "{w}");
        }
    }

    #[test]
    fn running_text_protects_short_words() {
        let fx = Fixture::new();
        let targets = fx.targets();
        // "db" alone reads as a mistyped Ukrainian "ви"; after an English
        // word it is the database it looks like.
        let keys = keys_of(&fx, Lang::En, "db").unwrap();
        for (continues, want) in [(true, true), (false, false)] {
            let ctx = Context {
                models: model::load().unwrap(),
                targets: &targets,
                bias: 0.0,
                continues,
                rejected: &never,
            };
            let kept = boundary(&ctx, &keys, &fx.us, Lang::En, false) == Verdict::Keep;
            assert_eq!(kept, want, "\"db\" with continues={continues}");
        }
    }

    fn partial_verdict(intended: Lang, word: &str, on: Lang) -> Option<(Lang, String)> {
        let fx = Fixture::new();
        let targets = fx.targets();
        let ctx = Context {
            models: model::load().unwrap(),
            targets: &targets,
            bias: bias_for(intended),
            continues: false,
            rejected: &never,
        };
        let keys = keys_of(&fx, intended, word).unwrap();
        partial(&ctx, &keys, fx.map(on), on).map(|(i, t)| (targets[i].lang, t))
    }

    #[test]
    fn partial_fires_on_clear_mistakes() {
        assert_eq!(
            partial_verdict(Lang::Ru, "прив", Lang::En),
            Some((Lang::Ru, "прив".into()))
        );
        assert_eq!(
            partial_verdict(Lang::En, "hello", Lang::Ru),
            Some((Lang::En, "hello".into()))
        );
    }

    #[test]
    fn partial_stays_quiet_on_the_field_log_misfires() {
        // Each of these fired mid-word before: real words (or their starts)
        // typed on the right layout.
        for (w, lang) in [
            ("руку", Lang::Ru),
            ("купу", Lang::Uk),
            ("купу", Lang::Ru),
            ("повы", Lang::Ru),
            ("довы", Lang::Ru),
        ] {
            assert_eq!(partial_verdict(lang, w, lang), None, "{w:?} on {lang:?}");
        }
    }

    #[test]
    fn convert_text_picks_the_right_cyrillic() {
        let fx = Fixture::new();
        let targets = fx.targets();
        let ctx = Context {
            models: model::load().unwrap(),
            targets: &targets,
            bias: 0.0,
            continues: false,
            rejected: &never,
        };
        let (out, t) = convert_text(&ctx, "ghbdsn? zr cghfdb&").unwrap();
        assert_eq!((out.as_str(), targets[t].lang), ("привіт, як справи?", Lang::Uk));
        let (out, t) = convert_text(&ctx, "ghbdtn? rfr ltkf").unwrap();
        assert_eq!((out.as_str(), targets[t].lang), ("привет, как дела", Lang::Ru));
        let (out, t) = convert_text(&ctx, "руддщ цщкдв").unwrap();
        assert_eq!((out.as_str(), targets[t].lang), ("hello world", Lang::En));
    }
}
