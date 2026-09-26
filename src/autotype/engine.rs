//! The keystroke state machine: words as they are typed, the corrections
//! applied to them, and the bookkeeping that keeps both in step with the
//! screen.
//!
//! Pure: every Win32 lookup is resolved before a key reaches it, and whatever
//! it wants done comes back as [`Effect`]s. That is what lets the whole
//! pipeline run in tests against a simulated application — including the
//! interesting failures, which only happen when the application is slow.
//!
//! A word is kept as the *keys* that typed it, not as text. The text on screen
//! is those keys through the layout they are shown in, and any other layout's
//! reading is one table lookup away — so a correction never has to guess what
//! a character "really" was, and the punctuation keys that are letters on the
//! Cyrillic layouts (`,` is б, `;` is ж) stay part of the word.

use super::decide::{self, Context, Target, Verdict};
use super::keymap::{Key, KeyMap, Lang, SCAN_CODES};
use super::model::Models;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type Hkl = isize;

/// An installed layout, with what each key types on it.
#[derive(Clone)]
pub struct Layout {
    pub hkl: Hkl,
    pub map: Arc<KeyMap>,
}

impl Layout {
    fn lang(&self) -> Option<Lang> {
        self.map.lang
    }
}

/// One key press, with everything the engine needs already looked up.
#[derive(Debug, Clone, Copy)]
pub struct KeyDown {
    pub vk: u16,
    pub sc: u8,
    /// Extended keys (arrows, the numpad Enter, right-hand modifiers) never
    /// type letters, whatever their scan code says.
    pub extended: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub win: bool,
    pub caps: bool,
    /// Layout of the thread that owns the focused window.
    pub hkl: Hkl,
    /// The focused window: typing into a different one starts from scratch.
    pub focus: isize,
    pub now: Instant,
}

/// What the hook does with the keystroke it is holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pass,
    /// Eat it: its effect is being produced some other way. The matching
    /// key-up is eaten too.
    Swallow,
}

/// Something to do to the outside world, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Delete `erase` characters before the caret, then type `text`.
    Replace { erase: usize, text: String },
    /// Type `text` as it is.
    Type(String),
    /// Press and release a virtual key (the Space, Enter or Tab held back).
    Key(u16),
    /// Ask the focused window to switch layout.
    Layout(Hkl),
}

// Virtual keys the engine looks at.
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_PAUSE: u16 = 0x13;
const VK_SPACE: u16 = 0x20;
const VK_PACKET: u16 = 0xE7;

fn is_modifier(vk: u16) -> bool {
    // Shift, Ctrl, Alt, CapsLock, both Win keys, the left/right variants.
    matches!(vk, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// A pause this long and the caret may be anywhere: whatever was being typed
/// is let go. Mid-word hesitation is far shorter; clicks and window changes
/// are caught directly and do not wait for this.
const IDLE_RESET: Duration = Duration::from_secs(10);

/// How long to keep typing on the application's behalf while it has not yet
/// applied a layout switch. Long enough for a busy application to get round
/// to its message queue; short enough that one which ignored the request
/// does not have its keys translated indefinitely.
const BRIDGE_MAX: Duration = Duration::from_millis(1500);

/// Earlier words kept for re-deciding along with a correction: "z ,s gjikf"
/// is one sentence on the wrong layout, and only its last word is long
/// enough to tell.
const WORD_HISTORY: usize = 3;
const RETRO_WORDS: usize = 2;

/// How far one word moves the Russian/Ukrainian context, on a scale of -1
/// (Russian) to 1 (Ukrainian).
const BIAS_STEP: f32 = 0.25;

/// What followed a finished word on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    Space,
    /// A printable key that is not part of words.
    Char(Key),
    /// Nothing: the word ended some other way (the user changed layout).
    Nothing,
}

#[derive(Debug, Clone, Default)]
struct Token {
    keys: Vec<Key>,
    /// Layout the keys are on screen in.
    hkl: Hkl,
    /// Set when a mid-word fix moved the word here from this layout.
    fixed_from: Option<Hkl>,
    /// Earlier words rewritten along with that fix.
    retro: usize,
    /// Not ours to touch: begun mid-word, or already settled by the user.
    hands_off: bool,
}

#[derive(Debug, Clone)]
struct Word {
    keys: Vec<Key>,
    hkl: Hkl,
    sep: Sep,
    hands_off: bool,
}

/// A correction the undo key can still take back: the last `words` entries,
/// rewritten from `from`, with nothing typed since.
#[derive(Debug, Clone)]
struct Fix {
    from: Hkl,
    words: usize,
    /// Ours rather than the user's: undoing it means they disagree, and the
    /// word is remembered.
    auto: bool,
}

struct Bridge {
    /// Layout the application still has.
    from: Hkl,
    /// Layout we asked for.
    to: Hkl,
    until: Instant,
}

/// Earlier words that follow a correction to the same layout.
#[derive(Default)]
struct Retro {
    count: usize,
    erase: usize,
    text: String,
}

pub struct Engine {
    layouts: Vec<Layout>,
    /// Scan codes that type part of a word on some installed layout.
    word_keys: [bool; SCAN_CODES],
    /// The layout each language was last actually typed on — where the user
    /// is sent, rather than whichever layout of that language comes first.
    preferred: Vec<(Lang, Hkl)>,
    token: Token,
    words: Vec<Word>,
    fix: Option<Fix>,
    bridge: Option<Bridge>,
    /// Last layout the application reported.
    observed: Hkl,
    /// Layout the last keystroke was typed for, and the one we last asked
    /// for: a change to anything else is the user switching by hand.
    typed_on: Hkl,
    requested: Option<Hkl>,
    swallowed: Option<u16>,
    focus: isize,
    last_key: Option<Instant>,
    bias: f32,
    rejected: HashSet<String>,
    /// Words newly sent back with the undo key, for the caller to persist.
    pub rejected_new: Vec<String>,
    /// The layout-conversion hotkey (RegisterHotKey modifiers, vk). Pressing
    /// it is not an edit: the word it is about to convert must survive it.
    pub hotkey: Option<(u32, u16)>,
    pub effects: Vec<Effect>,
}

impl Engine {
    pub fn new(layouts: Vec<Layout>) -> Self {
        let mut e = Engine {
            layouts: Vec::new(),
            word_keys: [false; SCAN_CODES],
            preferred: Vec::new(),
            token: Token::default(),
            words: Vec::new(),
            fix: None,
            bridge: None,
            observed: 0,
            typed_on: 0,
            requested: None,
            swallowed: None,
            focus: 0,
            last_key: None,
            bias: 0.0,
            rejected: HashSet::new(),
            rejected_new: Vec::new(),
            hotkey: None,
            effects: Vec::new(),
        };
        e.set_layouts(layouts);
        e
    }

    /// Replaces the installed layout list (after the user adds or removes one).
    pub fn set_layouts(&mut self, layouts: Vec<Layout>) {
        self.word_keys = [false; SCAN_CODES];
        for l in layouts.iter().filter(|l| l.lang().is_some()) {
            for sc in 0..SCAN_CODES {
                self.word_keys[sc] |= l.map.is_word_key(sc as u8);
            }
        }
        self.preferred.retain(|(_, h)| layouts.iter().any(|l| l.hkl == *h));
        self.layouts = layouts;
        self.reset();
    }

    /// Recent Cyrillic usage, -1 (Russian) to 1 (Ukrainian).
    pub fn bias(&self) -> f32 {
        self.bias
    }

    pub fn knows(&self, hkl: Hkl) -> bool {
        self.layouts.iter().any(|l| l.hkl == hkl)
    }

    pub fn reject(&mut self, word: &str) {
        self.rejected.insert(word.to_lowercase());
    }

    // ------------------------------------------------------------
    // Input
    // ------------------------------------------------------------

    pub fn key_down(&mut self, models: Option<&Models>, ev: &KeyDown) -> Action {
        if self.swallowed.is_some_and(|vk| vk != ev.vk) {
            // Its key-up is not coming after all: another key went down first.
            self.swallowed = None;
        }
        self.observed = ev.hkl;

        if is_modifier(ev.vk) {
            return Action::Pass;
        }
        if ev.vk == VK_PAUSE && !(ev.ctrl || ev.alt || ev.win) {
            let acted = self.manual(models);
            return self.swallow_if(ev.vk, acted);
        }
        if self.is_hotkey(ev) {
            return Action::Pass;
        }
        if ev.ctrl || ev.alt || ev.win || ev.vk == VK_PACKET {
            // Paste, undo, delete-word, a menu: the text changes or the caret
            // moves in ways the keys do not show.
            self.reset();
            return Action::Pass;
        }
        if ev.focus != self.focus || self.last_key.is_some_and(|t| ev.now - t > IDLE_RESET) {
            self.reset();
            self.focus = ev.focus;
        }
        self.last_key = Some(ev.now);

        let hkl = self.effective(ev.hkl, ev.now);
        let bridging = hkl != ev.hkl;
        let Some(layout) = self.layout(hkl).cloned() else {
            self.reset();
            return Action::Pass;
        };
        let Some(lang) = layout.lang() else {
            // A layout we have no words for: stay out of the way.
            self.reset();
            return Action::Pass;
        };
        self.prefer(lang, hkl);

        if hkl != self.typed_on {
            if self.requested == Some(hkl) {
                self.requested = None;
            } else if self.typed_on != 0 {
                // Alt+Shift, the language bar: the plainest statement of
                // intent there is, and the context follows it at once.
                self.lean(lang, 3.0);
            }
            self.typed_on = hkl;
        }
        if !self.token.keys.is_empty() && self.token.hkl != hkl {
            self.user_switched();
        }

        let key = Key {
            sc: ev.sc,
            shift: ev.shift,
            caps: ev.caps,
        };
        match ev.vk {
            VK_BACK => self.backspace(),
            VK_SPACE => self.boundary(models, Sep::Space, None, &layout, bridging, ev),
            VK_RETURN | VK_TAB => self.boundary(models, Sep::Nothing, Some(ev.vk), &layout, bridging, ev),
            _ if ev.extended => {
                self.reset();
                Action::Pass
            }
            _ => match layout.map.render(key) {
                Some(_) if self.word_keys[key.sc as usize] => {
                    self.letter(models, key, &layout, bridging, ev)
                }
                Some(c) if !c.is_control() => {
                    self.boundary(models, Sep::Char(key), None, &layout, bridging, ev)
                }
                _ => {
                    self.reset();
                    Action::Pass
                }
            },
        }
    }

    pub fn key_up(&mut self, vk: u16) -> Action {
        if self.swallowed == Some(vk) {
            self.swallowed = None;
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// A mouse button went down: the caret is wherever the user put it.
    pub fn click(&mut self) {
        self.reset();
    }

    /// The layout hotkey: undo the last correction, or convert the word just
    /// typed. `false` when there is nothing to act on — the caller then
    /// converts the selection instead.
    pub fn hotkey_convert(&mut self, models: Option<&Models>) -> bool {
        self.manual(models)
    }

    fn is_hotkey(&self, ev: &KeyDown) -> bool {
        self.hotkey.is_some_and(|(mods, vk)| {
            vk == ev.vk
                && (mods & 1 != 0) == ev.alt
                && (mods & 2 != 0) == ev.ctrl
                && (mods & 4 != 0) == ev.shift
                && (mods & 8 != 0) == ev.win
        })
    }

    fn swallow_if(&mut self, vk: u16, yes: bool) -> Action {
        if yes {
            self.swallowed = Some(vk);
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// Forget everything about the text around the caret.
    fn reset(&mut self) {
        self.token = Token::default();
        self.words.clear();
        self.fix = None;
    }

    // ------------------------------------------------------------
    // Layouts
    // ------------------------------------------------------------

    fn layout(&self, hkl: Hkl) -> Option<&Layout> {
        self.layouts.iter().find(|l| l.hkl == hkl)
    }

    fn prefer(&mut self, lang: Lang, hkl: Hkl) {
        match self.preferred.iter_mut().find(|(l, _)| *l == lang) {
            Some(p) => p.1 = hkl,
            None => self.preferred.push((lang, hkl)),
        }
    }

    /// One layout per language: the one last typed on, else the first installed.
    pub fn targets(&self) -> Vec<Layout> {
        Lang::ALL
            .into_iter()
            .filter_map(|lang| {
                let preferred = self
                    .preferred
                    .iter()
                    .find(|(l, _)| *l == lang)
                    .and_then(|(_, h)| self.layout(*h));
                preferred
                    .or_else(|| self.layouts.iter().find(|l| l.lang() == Some(lang)))
                    .cloned()
            })
            .collect()
    }

    /// The layout the user is typing for. Until the application applies a
    /// switch we asked for, that is the one we asked for, not the one it has.
    fn effective(&mut self, observed: Hkl, now: Instant) -> Hkl {
        if let Some(b) = &self.bridge {
            if observed == b.from && now < b.until {
                return b.to;
            }
            // Arrived — or the user went somewhere else, or the request was
            // ignored; either way the application decides again.
            self.bridge = None;
        }
        observed
    }

    fn switch_to(&mut self, to: Hkl, now: Instant) {
        self.effects.push(Effect::Layout(to));
        self.requested = Some(to);
        self.bridge = (to != self.observed).then(|| Bridge {
            from: self.observed,
            to,
            until: now + BRIDGE_MAX,
        });
    }

    /// The layout changed under a word being typed, and not by us: the user
    /// switched on purpose. What was typed is theirs; so is the rest of it.
    fn user_switched(&mut self) {
        let token = std::mem::take(&mut self.token);
        self.fix = None;
        self.words.clear();
        self.words.push(Word {
            keys: token.keys,
            hkl: token.hkl,
            sep: Sep::Nothing,
            hands_off: true,
        });
        self.token.hands_off = true;
    }

    /// Moves the Russian/Ukrainian context towards `lang`.
    fn lean(&mut self, lang: Lang, weight: f32) {
        let pull = match lang {
            Lang::Uk => 1.0,
            Lang::Ru => -1.0,
            Lang::En => return,
        };
        let step = (BIAS_STEP * weight).min(1.0);
        self.bias = self.bias * (1.0 - step) + pull * step;
    }

    // ------------------------------------------------------------
    // Keys
    // ------------------------------------------------------------

    fn letter(
        &mut self,
        models: Option<&Models>,
        key: Key,
        layout: &Layout,
        bridging: bool,
        ev: &KeyDown,
    ) -> Action {
        // Typing on after a finished correction accepts it; a correction made
        // inside this very word can still be taken back until it ends.
        if self.token.keys.is_empty() {
            self.fix = None;
            self.token.hkl = layout.hkl;
        }
        self.token.keys.push(key);

        let lang = layout.lang().unwrap_or(Lang::En);
        if let Some(models) = models
            && !self.token.hands_off
            && self.token.fixed_from.is_none()
        {
            let targets = self.targets();
            let found = self.with_context(models, &targets, false, |ctx, me| {
                let (t, text) = decide::partial(ctx, &me.token.keys, &layout.map, lang)?;
                Some((t, text, me.retro(ctx, layout, t)))
            });
            if let Some((t, text, retro)) = found {
                let to = &targets[t];
                let before = self.token.keys.len() - 1; // this key is not on screen yet
                println!(
                    "[punto] {} → {}{text} (mid-word, {})",
                    layout.map.render_all(&self.token.keys).unwrap_or_default(),
                    retro.text,
                    to.lang().map_or("?", Lang::code)
                );
                self.effects.push(Effect::Replace {
                    erase: retro.erase + before,
                    text: format!("{}{text}", retro.text),
                });
                self.switch_to(to.hkl, ev.now);
                self.move_words(retro.count, to.hkl);
                self.token.fixed_from = Some(layout.hkl);
                self.token.retro = retro.count;
                self.token.hkl = to.hkl;
                self.fix = Some(Fix {
                    from: layout.hkl,
                    words: 0,
                    auto: true,
                });
                self.swallowed = Some(ev.vk);
                return Action::Swallow;
            }
        }

        if bridging && let Some(c) = layout.map.render(key) {
            self.effects.push(Effect::Type(c.to_string()));
            self.swallowed = Some(ev.vk);
            return Action::Swallow;
        }
        Action::Pass
    }

    fn backspace(&mut self) -> Action {
        self.fix = None;
        if self.token.keys.pop().is_some() {
            if self.token.keys.is_empty() {
                self.token = Token::default();
            }
            return Action::Pass;
        }
        // Nothing in progress: the backspace eats whatever followed the last
        // word, and the caret is back at its end — typing continues it.
        if let Some(w) = self.words.pop() {
            let mut keys = w.keys;
            if w.sep == Sep::Nothing {
                keys.pop();
            }
            if !keys.is_empty() {
                self.token = Token {
                    keys,
                    hkl: w.hkl,
                    hands_off: w.hands_off,
                    ..Token::default()
                };
            }
        }
        Action::Pass
    }

    /// Space, Enter, Tab or punctuation after a word: decide the word.
    ///
    /// `replay` is set for Enter and Tab, keys with a side effect (a message
    /// sent, focus moved) that must happen *after* the fix: they are held
    /// back and pressed again once it is in.
    fn boundary(
        &mut self,
        models: Option<&Models>,
        sep: Sep,
        replay: Option<u16>,
        layout: &Layout,
        bridging: bool,
        ev: &KeyDown,
    ) -> Action {
        let token = std::mem::take(&mut self.token);
        let mid_fix = self.fix.take().filter(|_| token.fixed_from.is_some());
        let Some(shown) = self.layout(token.hkl).cloned().filter(|_| !token.keys.is_empty()) else {
            // A separator after a separator: nothing is adjacent any more.
            self.words.clear();
            return self.separator(sep, layout, bridging, ev);
        };
        let lang = shown.lang().unwrap_or(Lang::En);
        let targets = self.targets();

        let continues = self
            .words
            .last()
            .is_some_and(|w| w.hkl == token.hkl && w.sep != Sep::Nothing);
        let decided = match models {
            Some(models) if !token.hands_off => self.with_context(models, &targets, continues, |ctx, me| {
                let v = decide::boundary(ctx, &token.keys, &shown.map, lang, token.fixed_from.is_some());
                let retro = match &v {
                    Verdict::Convert { target, .. } => me.retro(ctx, &shown, *target),
                    _ => Retro::default(),
                };
                Some((v, retro))
            }),
            _ => None,
        };
        let (verdict, retro) = decided.unwrap_or((Verdict::Keep, Retro::default()));

        match verdict {
            Verdict::Convert { target, text } => {
                let to = &targets[target];
                // Punctuation is retyped as the target layout has it: "/" on
                // an English layout is the "." the user reached for.
                let sep_text = match sep {
                    Sep::Char(k) => to.map.render(k).map(String::from).unwrap_or_default(),
                    _ => String::new(),
                };
                println!(
                    "[punto] {} → {}{text} ({})",
                    shown.map.render_all(&token.keys).unwrap_or_default(),
                    retro.text,
                    to.lang().map_or("?", Lang::code)
                );
                self.effects.push(Effect::Replace {
                    erase: retro.erase + token.keys.len(),
                    text: format!("{}{text}{sep_text}", retro.text),
                });
                match (sep, replay) {
                    (Sep::Space, _) => self.effects.push(Effect::Key(VK_SPACE)),
                    (_, Some(vk)) => self.effects.push(Effect::Key(vk)),
                    _ => {}
                }
                self.switch_to(to.hkl, ev.now);
                if let Some(l) = to.lang() {
                    self.lean(l, 1.0);
                }
                self.move_words(retro.count, to.hkl);
                let from = token.fixed_from.unwrap_or(token.hkl);
                let words = retro.count.max(token.retro) + 1;
                self.finish(token.keys, to.hkl, sep, replay, false);
                if replay.is_none() {
                    self.fix = Some(Fix {
                        from,
                        words,
                        auto: true,
                    });
                }
                self.swallowed = Some(ev.vk);
                Action::Swallow
            }
            Verdict::Relayout { target } => {
                let to = &targets[target];
                println!(
                    "[punto] {} stays, layout → {}",
                    shown.map.render_all(&token.keys).unwrap_or_default(),
                    to.lang().map_or("?", Lang::code)
                );
                self.switch_to(to.hkl, ev.now);
                if let Some(l) = to.lang() {
                    self.lean(l, 1.0);
                }
                let from = token.hkl;
                self.finish(token.keys, to.hkl, sep, replay, false);
                if replay.is_none() {
                    self.fix = Some(Fix {
                        from,
                        words: 1,
                        auto: true,
                    });
                }
                // Both Cyrillic layouts type the same punctuation, so the key
                // itself can go through.
                self.separator(sep, layout, bridging, ev)
            }
            Verdict::Keep => {
                self.lean(lang, 1.0);
                let words = token.retro + 1;
                let from = token.fixed_from;
                self.finish(token.keys, token.hkl, sep, replay, token.hands_off);
                // A mid-word fix the word went on to confirm is still the
                // last correction: the undo key takes back word and all.
                if let (Some(from), Some(_), None) = (from, mid_fix, replay) {
                    self.fix = Some(Fix {
                        from,
                        words,
                        auto: true,
                    });
                }
                self.separator(sep, layout, bridging, ev)
            }
        }
    }

    /// Records a finished word. Enter and Tab end the line: there is no
    /// going back into it by keystrokes, so nothing is kept.
    fn finish(&mut self, keys: Vec<Key>, hkl: Hkl, sep: Sep, replay: Option<u16>, hands_off: bool) {
        if replay.is_some() {
            self.words.clear();
            return;
        }
        self.words.push(Word {
            keys,
            hkl,
            sep,
            hands_off,
        });
        if self.words.len() > WORD_HISTORY {
            self.words.remove(0);
        }
    }

    /// Lets a separator through — or, while the application is still on the
    /// old layout, types it as the new one would have.
    fn separator(&mut self, sep: Sep, layout: &Layout, bridging: bool, ev: &KeyDown) -> Action {
        if bridging
            && let Sep::Char(k) = sep
            && let Some(c) = layout.map.render(k)
        {
            self.effects.push(Effect::Type(c.to_string()));
            self.swallowed = Some(ev.vk);
            return Action::Swallow;
        }
        Action::Pass
    }

    /// The last `n` finished words now read in another layout.
    fn move_words(&mut self, n: usize, to: Hkl) {
        let start = self.words.len() - n;
        for w in &mut self.words[start..] {
            w.hkl = to;
        }
    }

    /// Earlier words to rewrite along with a correction to `targets[target]`:
    /// typed on the same layout, one space apart, and real words there.
    fn retro(&self, ctx: &Context, shown: &Layout, target: usize) -> Retro {
        let mut out = Retro::default();
        let Some(lang) = shown.lang() else {
            return out;
        };
        for w in self.words.iter().rev().take(RETRO_WORDS) {
            if w.hkl != shown.hkl || w.sep != Sep::Space || w.hands_off {
                break;
            }
            let Some(text) = decide::retro(ctx, &w.keys, &shown.map, lang, target) else {
                break;
            };
            out.count += 1;
            out.erase += w.keys.len() + 1;
            out.text = format!("{text} {}", out.text);
        }
        out
    }

    /// Runs `f` with a decision context over `targets`.
    fn with_context<T>(
        &self,
        models: &Models,
        targets: &[Layout],
        continues: bool,
        f: impl FnOnce(&Context, &Self) -> Option<T>,
    ) -> Option<T> {
        let list: Vec<Target> = targets
            .iter()
            .filter_map(|l| {
                Some(Target {
                    lang: l.lang()?,
                    map: &l.map,
                })
            })
            .collect();
        let rejected = |w: &str| {
            self.rejected.contains(w) || self.rejected.iter().any(|r| r.starts_with(w))
        };
        let ctx = Context {
            models,
            targets: &list,
            bias: self.bias,
            continues,
            rejected: &rejected,
        };
        f(&ctx, self)
    }

    // ------------------------------------------------------------
    // The undo key
    // ------------------------------------------------------------

    /// Pause (or the layout hotkey): take back the correction just made, or
    /// convert the word just typed. Pressed again, it goes back.
    fn manual(&mut self, models: Option<&Models>) -> bool {
        // The word still being typed.
        if !self.token.keys.is_empty() {
            let Some(shown) = self.layout(self.token.hkl).cloned() else {
                return false;
            };
            let (to, auto) = match self.token.fixed_from {
                Some(from) => (Some(from), true),
                None => (self.forced_target(models, &self.token.keys, &shown), false),
            };
            let Some(to) = to.and_then(|h| self.layout(h).cloned()) else {
                return false;
            };
            // Earlier words fixed along with it go back too.
            let retro = if auto { self.token.retro } else { 0 };
            let start = self.words.len() - retro;
            let (erase, mut text) = self.span(&self.words[start..], &to.map);
            let word = to.map.render_all(&self.token.keys).unwrap_or_default();
            if auto {
                println!("[punto] undo: {word}");
                self.remember_rejected(&word);
            } else {
                println!("[punto] manual: {word}");
            }
            text.push_str(&word);
            self.effects.push(Effect::Replace {
                erase: erase + self.token.keys.len(),
                text,
            });
            self.effects.push(Effect::Layout(to.hkl));
            self.bridge = None;
            self.move_words(retro, to.hkl);
            self.token.hkl = to.hkl;
            self.token.fixed_from = None;
            self.token.retro = 0;
            self.token.hands_off = true;
            self.fix = None;
            return true;
        }

        // The correction just made, the word(s) and separator it covered.
        if let Some(fix) = self.fix.take() {
            let Some(to) = self.layout(fix.from).cloned() else {
                return false;
            };
            let n = fix.words.min(self.words.len());
            let start = self.words.len() - n;
            let Some(shown) = self.words.last().and_then(|w| self.layout(w.hkl)).cloned() else {
                return false;
            };
            let (erase, text) = self.span(&self.words[start..], &to.map);
            let word = to
                .map
                .render_all(&self.words[self.words.len() - 1].keys)
                .unwrap_or_default();
            if fix.auto {
                println!("[punto] undo: {text}");
                self.remember_rejected(&word);
            } else {
                println!("[punto] back: {text}");
            }
            if self.span(&self.words[start..], &shown.map).1 != text {
                self.effects.push(Effect::Replace { erase, text });
            }
            self.effects.push(Effect::Layout(to.hkl));
            self.bridge = None;
            for w in &mut self.words[start..] {
                w.hkl = to.hkl;
                w.hands_off = true;
            }
            return true;
        }

        // The word just finished, untouched so far: convert it by hand.
        if let Some(w) = self.words.last().cloned()
            && w.sep != Sep::Nothing
            && let Some(shown) = self.layout(w.hkl).cloned()
            && let Some(to) = self
                .forced_target(models, &w.keys, &shown)
                .and_then(|h| self.layout(h).cloned())
        {
            let start = self.words.len() - 1;
            let (erase, text) = self.span(&self.words[start..], &to.map);
            println!("[punto] manual: {text}");
            self.effects.push(Effect::Replace { erase, text });
            self.effects.push(Effect::Layout(to.hkl));
            self.bridge = None;
            let last = self.words.last_mut().unwrap();
            last.hkl = to.hkl;
            last.hands_off = true;
            self.fix = Some(Fix {
                from: shown.hkl,
                words: 1,
                auto: false,
            });
            return true;
        }
        false
    }

    /// Where the undo key sends a word nobody corrected: the likeliest other
    /// layout, however unsure.
    fn forced_target(&self, models: Option<&Models>, keys: &[Key], shown: &Layout) -> Option<Hkl> {
        let targets = self.targets();
        let lang = shown.lang()?;
        let pick = match models {
            Some(models) => self.with_context(models, &targets, false, |ctx, _| {
                decide::forced(ctx, keys, lang).map(|(t, _)| t)
            }),
            None => None,
        };
        // Without word models: Latin goes to the Cyrillic layout, Cyrillic to Latin.
        let pick = pick.or_else(|| {
            targets
                .iter()
                .position(|t| t.lang().is_some_and(|l| l.is_cyrillic() != lang.is_cyrillic()))
        })?;
        Some(targets[pick].hkl)
    }

    /// `words` and their separators, as on screen: how many characters they
    /// take, and the text the same keys make through `to`.
    fn span(&self, words: &[Word], to: &KeyMap) -> (usize, String) {
        let mut erase = 0;
        let mut text = String::new();
        for w in words {
            erase += w.keys.len();
            text.push_str(&to.render_all(&w.keys).unwrap_or_default());
            match w.sep {
                Sep::Space => {
                    erase += 1;
                    text.push(' ');
                }
                Sep::Char(k) => {
                    erase += 1;
                    if let Some(c) = to.render(k) {
                        text.push(c);
                    }
                }
                Sep::Nothing => {}
            }
        }
        (erase, text)
    }

    fn remember_rejected(&mut self, word: &str) {
        let w = word.to_lowercase();
        if self.rejected.insert(w.clone()) {
            self.rejected_new.push(w);
        }
    }
}

// ============================================================
// Tests: the engine against a simulated application
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotype::model;

    const EN: Hkl = 0x0409_0409;
    const RU: Hkl = 0x0419_0419;
    const UK: Hkl = 0xF0A8_0422u32 as i32 as isize;
    const RU_UA: Hkl = 0x0419_2000;

    fn layouts() -> Vec<Layout> {
        vec![
            Layout {
                hkl: EN,
                map: Arc::new(KeyMap::us()),
            },
            Layout {
                hkl: RU,
                map: Arc::new(KeyMap::ru()),
            },
            Layout {
                hkl: UK,
                map: Arc::new(KeyMap::uk()),
            },
            Layout {
                hkl: RU_UA,
                map: Arc::new(KeyMap::ru()),
            },
        ]
    }

    /// An application: a line of text and a layout, which it applies only
    /// `latency` keystrokes after being asked to.
    struct Sim {
        engine: Engine,
        doc: String,
        layout: Hkl,
        pending: Option<(Hkl, usize)>,
        latency: usize,
        now: Instant,
    }

    impl Sim {
        fn new(layout: Hkl) -> Self {
            Sim {
                engine: Engine::new(layouts()),
                doc: String::new(),
                layout,
                pending: None,
                latency: 0,
                now: Instant::now(),
            }
        }

        fn latency(mut self, n: usize) -> Self {
            self.latency = n;
            self
        }

        fn map(&self) -> Arc<KeyMap> {
            self.engine.layout(self.layout).unwrap().map.clone()
        }

        fn apply(&mut self) {
            for e in std::mem::take(&mut self.engine.effects) {
                match e {
                    Effect::Replace { erase, text } => {
                        for _ in 0..erase {
                            self.doc.pop();
                        }
                        self.doc.push_str(&text);
                    }
                    Effect::Type(t) => self.doc.push_str(&t),
                    Effect::Key(VK_SPACE) => self.doc.push(' '),
                    Effect::Key(VK_RETURN) => self.doc.push('\n'),
                    Effect::Key(_) => {}
                    Effect::Layout(h) => self.pending = Some((h, self.latency)),
                }
            }
        }

        fn press(&mut self, vk: u16, key: Key, mods: (bool, bool)) {
            // The application acts on a layout request when it gets to it.
            if let Some((h, n)) = self.pending {
                if n == 0 {
                    self.layout = h;
                    self.pending = None;
                } else {
                    self.pending = Some((h, n - 1));
                }
            }
            self.now += Duration::from_millis(80);
            let ev = KeyDown {
                vk,
                sc: key.sc,
                extended: false,
                shift: key.shift,
                ctrl: mods.0,
                alt: mods.1,
                win: false,
                caps: false,
                hkl: self.layout,
                focus: 1,
                now: self.now,
            };
            let action = self.engine.key_down(model::load(), &ev);
            self.apply();
            if action == Action::Pass && !(mods.0 || mods.1) {
                match vk {
                    VK_BACK => {
                        self.doc.pop();
                    }
                    VK_SPACE => self.doc.push(' '),
                    VK_RETURN => self.doc.push('\n'),
                    VK_PAUSE => {}
                    _ => {
                        if let Some(c) = self.map().render(key) {
                            self.doc.push(c);
                        }
                    }
                }
            }
            self.engine.key_up(vk);
        }

        /// Types `text` as the keys that produce it on `as_if` — what the user's
        /// fingers do when they believe that layout is active.
        fn type_as(&mut self, as_if: &KeyMap, text: &str) {
            for ch in text.chars() {
                match ch {
                    ' ' => self.space(),
                    '\n' => self.press(VK_RETURN, Key { sc: 0x1C, shift: false, caps: false }, (false, false)),
                    _ => {
                        let key = as_if.key_for(ch).unwrap_or_else(|| panic!("{ch:?} not typeable"));
                        // Letters' virtual keys don't matter to the engine,
                        // beyond not being one of the special ones.
                        self.press(0x41, key, (false, false));
                    }
                }
            }
        }

        fn space(&mut self) {
            self.press(VK_SPACE, Key { sc: 0x39, shift: false, caps: false }, (false, false));
        }

        fn backspace(&mut self) {
            self.press(VK_BACK, Key { sc: 0x0E, shift: false, caps: false }, (false, false));
        }

        fn pause(&mut self) {
            self.press(VK_PAUSE, Key { sc: 0x45, shift: false, caps: false }, (false, false));
        }

        fn ctrl(&mut self, vk: u16) {
            self.press(vk, Key { sc: 0x2F, shift: false, caps: false }, (true, false));
        }

        /// Lets any requested layout switch land, as idle time would.
        fn settle(&mut self) -> &str {
            if let Some((h, _)) = self.pending.take() {
                self.layout = h;
            }
            &self.doc
        }
    }

    fn us() -> KeyMap {
        KeyMap::us()
    }
    fn ru() -> KeyMap {
        KeyMap::ru()
    }
    fn uk() -> KeyMap {
        KeyMap::uk()
    }

    #[test]
    fn russian_typed_on_english() {
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "привет как дела ");
        assert_eq!(sim.settle(), "привет как дела ");
        assert_eq!(sim.layout, RU);
    }

    #[test]
    fn english_typed_on_russian() {
        let mut sim = Sim::new(RU);
        sim.type_as(&us(), "hello world ");
        assert_eq!(sim.settle(), "hello world ");
        assert_eq!(sim.layout, EN);
    }

    #[test]
    fn a_slow_application_does_not_garble_the_rest() {
        // The switch lands several keystrokes late; until then the engine
        // types for the application.
        for latency in 0..=6 {
            let mut sim = Sim::new(RU).latency(latency);
            sim.type_as(&us(), "curtains and more ");
            assert_eq!(sim.settle(), "curtains and more ", "latency {latency}");
        }
    }

    #[test]
    fn letters_on_punctuation_keys_stay_in_the_word() {
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "большими буквами ");
        assert_eq!(sim.settle(), "большими буквами ");
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "бесконечную ");
        assert_eq!(sim.settle(), "бесконечную ");
    }

    #[test]
    fn punctuation_is_retyped_on_the_target_layout() {
        // On the Russian layout "." and "," are the keys English has "/" and
        // "?" on; typing "привет." on the wrong layout gives "ghbdtn/".
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "привет. как дела, друг?");
        assert_eq!(sim.settle(), "привет. как дела, друг?");
    }

    #[test]
    fn english_punctuation_after_a_word_on_russian() {
        let mut sim = Sim::new(RU);
        sim.type_as(&us(), "hello, world. ");
        assert_eq!(sim.settle(), "hello, world. ");
    }

    #[test]
    fn enter_is_pressed_after_the_fix() {
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "привет\n");
        assert_eq!(sim.settle(), "привет\n");
    }

    #[test]
    fn russian_ukraine_layout_is_russian() {
        // "Russian (Ukraine)" has a transient language ID; it must still read
        // as Russian, and corrections from English go back to it.
        let mut sim = Sim::new(RU_UA);
        sim.type_as(&ru(), "привет ");
        assert_eq!(sim.settle(), "привет ");
        assert_eq!(sim.layout, RU_UA, "a correct Russian word stays put");
        sim.type_as(&us(), "hello ");
        assert_eq!(sim.settle(), "привет hello ");
        assert_eq!(sim.layout, EN);
        sim.type_as(&ru(), "мир ");
        assert_eq!(sim.settle(), "привет hello мир ");
        assert_eq!(sim.layout, RU_UA, "back to the Russian layout the user uses");
    }

    #[test]
    fn ukrainian_and_russian_layouts() {
        let mut sim = Sim::new(RU);
        sim.type_as(&uk(), "привіт ");
        assert_eq!(sim.settle(), "привіт ");
        assert_eq!(sim.layout, UK);
        let mut sim = Sim::new(UK);
        sim.type_as(&ru(), "это мы ");
        assert_eq!(sim.settle(), "это мы ");
        assert_eq!(sim.layout, RU);
    }

    #[test]
    fn short_words_follow_the_sentence() {
        // "z ,s gjikf" alone: "z" and ",s" could be anything; the last word
        // settles it and they come along.
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "я бы пошла ");
        assert_eq!(sim.settle(), "я бы пошла ");
    }

    #[test]
    fn ambiguous_word_follows_the_sentence() {
        // "была" on the Ukrainian layout is "біла", a Ukrainian word in its
        // own right, and stays — until "это" (typed as "єто") shows which
        // language the sentence is in.
        let mut sim = Sim::new(UK);
        sim.type_as(&ru(), "была это ");
        assert_eq!(sim.settle(), "была это ");
        assert_eq!(sim.layout, RU);
    }

    #[test]
    fn real_words_are_left_alone() {
        let mut sim = Sim::new(EN);
        sim.type_as(&us(), "the quick brown fox jumps over the lazy dog ");
        assert_eq!(sim.settle(), "the quick brown fox jumps over the lazy dog ");
        let mut sim = Sim::new(RU);
        sim.type_as(&ru(), "дай мне руку и пойдём ");
        assert_eq!(sim.settle(), "дай мне руку и пойдём ");
        let mut sim = Sim::new(UK);
        sim.type_as(&uk(), "оце так новина ");
        assert_eq!(sim.settle(), "оце так новина ");
    }

    #[test]
    fn pause_takes_a_correction_back_and_remembers() {
        let mut sim = Sim::new(RU);
        sim.type_as(&us(), "hello ");
        assert_eq!(sim.settle(), "hello ");
        sim.pause();
        assert_eq!(sim.settle(), "руддщ ");
        assert_eq!(sim.layout, RU);
        // The same word is not touched again.
        sim.type_as(&us(), "hello ");
        assert_eq!(sim.settle(), "руддщ руддщ ");
        assert_eq!(sim.engine.rejected_new, vec!["руддщ".to_string()]);
    }

    #[test]
    fn pause_converts_a_word_nobody_touched_and_toggles() {
        let mut sim = Sim::new(EN);
        // "hjc" is not decisive on its own; the user says it is Russian.
        sim.type_as(&us(), "ok ");
        sim.pause();
        assert_eq!(sim.settle(), "щл ");
        sim.pause();
        assert_eq!(sim.settle(), "ok ");
    }

    #[test]
    fn pause_mid_word() {
        // Three letters are too few to decide on; the user does it for us.
        let mut sim = Sim::new(EN);
        sim.type_as(&us(), "ghb");
        sim.pause();
        assert_eq!(sim.settle(), "при");
        assert_eq!(sim.layout, RU);
        sim.type_as(&ru(), "вет ");
        assert_eq!(sim.settle(), "привет ");
    }

    #[test]
    fn pause_takes_back_a_mid_word_fix() {
        let mut sim = Sim::new(RU);
        sim.type_as(&us(), "hello");
        assert_eq!(sim.settle(), "hello", "the mid-word fix");
        sim.pause();
        assert_eq!(sim.settle(), "руддщ");
        assert_eq!(sim.layout, RU);
        sim.type_as(&ru(), "ы ");
        assert_eq!(sim.settle(), "руддщы ", "and the word is left alone from then on");
    }

    #[test]
    fn backspace_inside_a_word() {
        let mut sim = Sim::new(EN);
        sim.type_as(&ru(), "приветт");
        sim.backspace();
        sim.space();
        assert_eq!(sim.settle(), "привет ");
    }

    #[test]
    fn a_shortcut_ends_the_word() {
        // Ctrl+V put text we never saw between the two halves: they must not
        // be treated as one word.
        let mut sim = Sim::new(EN);
        sim.type_as(&us(), "ghb");
        sim.ctrl(0x56);
        sim.type_as(&us(), "dtn ");
        assert!(sim.settle().starts_with("ghb"), "nothing may be rewritten across the paste");
    }

    #[test]
    fn switching_layout_by_hand_mid_word_is_respected() {
        let mut sim = Sim::new(EN);
        sim.type_as(&us(), "gh");
        sim.layout = RU; // Alt+Shift
        sim.type_as(&ru(), "ивет ");
        assert_eq!(sim.settle(), "ghивет ");
    }

    #[test]
    fn relayout_moves_a_ukrainian_writer_off_the_russian_layout() {
        let mut sim = Sim::new(RU);
        sim.type_as(&ru(), "купувати ");
        assert_eq!(sim.settle(), "купувати ");
        assert_eq!(sim.layout, UK);
        sim.type_as(&uk(), "хліб ");
        assert_eq!(sim.settle(), "купувати хліб ");
    }
}
