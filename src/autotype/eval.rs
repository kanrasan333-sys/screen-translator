//! Accuracy measurement against held-out word lists.
//!
//!     python tools/build_punto_dicts.py --eval <dir>
//!     set PUNTO_EVAL_DIR=<dir>
//!     cargo test --release eval -- --ignored --nocapture
//!
//! For every word of every language it asks two questions. Typed on its own
//! layout, is it left alone? Typed on each of the others, is it restored —
//! to the right language, letter for letter? `typical` lists are drawn by
//! frequency, so their rates are per word *typed*; `uniform` is spread across
//! the whole dictionary and `rare` lies beyond it, which is where guessing
//! takes over from lookup.

use super::decide::tests::{Fixture, bias_for, keys_of, never};
use super::decide::{self, Context, Verdict};
use super::keymap::Lang;
use super::model;
use std::collections::BTreeMap;

#[derive(Default)]
struct Tally {
    total: usize,
    good: usize,
    wrong: usize,
    samples: BTreeMap<String, usize>,
}

impl Tally {
    fn add(&mut self, good: bool, wrong: bool, sample: impl FnOnce() -> String) {
        self.total += 1;
        if good {
            self.good += 1;
        } else {
            if wrong {
                self.wrong += 1;
            }
            if self.samples.len() < 400 {
                *self.samples.entry(sample()).or_default() += 1;
            }
        }
    }
    fn pct(n: usize, d: usize) -> f64 {
        100.0 * n as f64 / d.max(1) as f64
    }
}

#[test]
#[ignore]
fn eval() {
    let Ok(dir) = std::env::var("PUNTO_EVAL_DIR") else {
        eprintln!("PUNTO_EVAL_DIR not set");
        return;
    };
    let models = model::load().unwrap();
    let fx = Fixture::new();
    let targets = fx.targets();
    // A user writing Russian has been writing Russian: the context bias the
    // engine keeps is set the way it would be by then.
    // PUNTO_EVAL_BIAS=zero|against: no context, or the wrong one — the first
    // Ukrainian words right after a Russian conversation.
    let mode = std::env::var("PUNTO_EVAL_BIAS").unwrap_or_default();
    let ctx_for = |intended: Lang| Context {
        models,
        targets: &targets,
        bias: match mode.as_str() {
            "zero" => 0.0,
            "against" => -bias_for(intended) / 0.7,
            _ => bias_for(intended),
        },
        continues: false,
        rejected: &never,
    };
    let show_samples = std::env::var("PUNTO_EVAL_SAMPLES").is_ok();

    for list in ["typical", "uniform", "rare"] {
        println!("\n=== {list} ===");
        for intended in Lang::ALL {
            let path = format!("{dir}/{}_{list}.txt", intended.code());
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let words: Vec<&str> = text.lines().filter(|w| !w.is_empty()).collect();
            let ctx = ctx_for(intended);
            for on in Lang::ALL {
                let mut boundary = Tally::default();
                let mut relayout = Tally::default();
                let mut mid = Tally::default();
                for &w in &words {
                    let Some(keys) = keys_of(&fx, intended, w) else {
                        continue;
                    };
                    let shown = fx.map(on);
                    let typed = shown.render_all(&keys).unwrap_or_default();
                    let v = decide::boundary(&ctx, &keys, shown, on, false);

                    // Mid-word: the first prefix that fires, if any.
                    let mut fired = None;
                    for n in decide::PARTIAL_MIN..=decide::PARTIAL_MAX.min(keys.len().saturating_sub(1)) {
                        if let Some((t, text)) = decide::partial(&ctx, &keys[..n], shown, on) {
                            fired = Some((targets[t].lang, text, n));
                            break;
                        }
                    }

                    if on == intended {
                        boundary.add(v == Verdict::Keep, true, || format!("{w} -> {v:?}"));
                        mid.add(fired.is_none(), true, || format!("{w} -> {fired:?}"));
                    } else if typed == w {
                        let ok = matches!(v, Verdict::Relayout { target } if targets[target].lang == intended);
                        let bad = matches!(v, Verdict::Convert { .. })
                            || matches!(v, Verdict::Relayout { target } if targets[target].lang != intended);
                        relayout.add(ok, bad, || format!("{w} -> {v:?}"));
                    } else {
                        let ok = matches!(&v, Verdict::Convert { target, text } if targets[*target].lang == intended && text == w);
                        let bad = matches!(v, Verdict::Convert { .. } | Verdict::Relayout { .. }) && !ok;
                        boundary.add(ok, bad, || format!("{typed} ({w}) -> {v:?}"));
                        if let Some((lang, text, n)) = &fired {
                            let good = *lang == intended && w.starts_with(text.as_str()) && text.chars().count() == *n;
                            mid.add(good, !good, || format!("{typed} ({w}) mid -> {lang:?} {text}"));
                        } else {
                            mid.total += 1; // silent: the boundary gets it
                        }
                    }
                }
                if on == intended {
                    println!(
                        "{:>2} on {:>2}: {:>6} words  boundary FP {:6.3}%  mid-word FP {:6.3}%",
                        intended.code(),
                        on.code(),
                        boundary.total,
                        Tally::pct(boundary.total - boundary.good, boundary.total),
                        Tally::pct(mid.total - mid.good, mid.total),
                    );
                } else {
                    println!(
                        "{:>2} on {:>2}: {:>6} words  fixed {:6.2}%  wrong {:5.2}%  | same text {:>5}: relayout {:6.2}% wrong {:5.2}%  | mid-word fired {:6.2}% wrong {:5.2}%",
                        intended.code(),
                        on.code(),
                        boundary.total,
                        Tally::pct(boundary.good, boundary.total),
                        Tally::pct(boundary.wrong, boundary.total),
                        relayout.total,
                        Tally::pct(relayout.good, relayout.total),
                        Tally::pct(relayout.wrong, relayout.total),
                        Tally::pct(mid.good + mid.wrong, mid.total),
                        Tally::pct(mid.wrong, mid.total),
                    );
                }
                if show_samples {
                    for (name, t) in [("boundary", &boundary), ("relayout", &relayout), ("mid", &mid)] {
                        let mut s: Vec<_> = t.samples.iter().collect();
                        s.sort_by(|a, b| b.1.cmp(a.1));
                        let shown: Vec<String> =
                            s.iter().take(25).map(|(k, n)| format!("{k} x{n}")).collect();
                        if !shown.is_empty() {
                            println!("    {name}: {}", shown.join(" | "));
                        }
                    }
                }
            }
        }
    }
}

/// Score breakdown for words named in PUNTO_EXPLAIN="ru:ну:en,en:the:ru".
#[test]
#[ignore]
fn explain() {
    let Ok(spec) = std::env::var("PUNTO_EXPLAIN") else {
        return;
    };
    let models = model::load().unwrap();
    let fx = Fixture::new();
    for item in spec.split(',') {
        let f: Vec<&str> = item.split(':').collect();
        let lang = |s: &str| Lang::ALL.into_iter().find(|l| l.code() == s).unwrap();
        let (intended, word, on) = (lang(f[0]), f[1], lang(f[2]));
        let keys = keys_of(&fx, intended, word).unwrap();
        println!("{word} ({intended:?}) typed on {on:?}:");
        for l in Lang::ALL {
            let text = fx.map(l).render_all(&keys).unwrap_or_default();
            let m = models.get(l);
            let mut buf = Vec::new();
            let enc = m.encode(&text, &mut buf);
            let (lp, p) = if enc { m.word_log2(&buf) } else { (f32::NAN, None) };
            let shape = if enc { m.trigram_log2(&buf, true) / (buf.len() + 1) as f32 } else { f32::NAN };
            println!("   {l:?} {text:12} log2 {lp:7.2}  p {p:?}  shape {shape:.2}");
        }
        let targets = fx.targets();
        let ctx = Context {
            models,
            targets: &targets,
            bias: bias_for(intended),
            continues: false,
            rejected: &never,
        };
        let v = decide::boundary(&ctx, &keys, fx.map(on), on, false);
        println!("   => {v:?}");
    }
}

/// Types this repository's own source code on the English layout, token by
/// token as the engine splits it, and counts what would be "corrected".
#[test]
#[ignore]
fn code() {
    let models = model::load().unwrap();
    let fx = Fixture::new();
    let targets = fx.targets();
    let word_key = |sc: u8| [&fx.us, &fx.ru, &fx.uk].iter().any(|m| m.is_word_key(sc));
    let mut total = 0usize;
    let mut fired: BTreeMap<String, usize> = BTreeMap::new();
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir("src")
        .unwrap()
        .chain(std::fs::read_dir("src/autotype").unwrap())
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.push("README.md".into());
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let mut keys = Vec::new();
        let mut prev_kept = false;
        let flush = |keys: &mut Vec<_>, prev_kept: &mut bool, total: &mut usize, fired: &mut BTreeMap<String, usize>| {
            if keys.is_empty() {
                *prev_kept = false;
                return;
            }
            *total += 1;
            let ctx = Context {
                models,
                targets: &targets,
                bias: 0.0,
                continues: *prev_kept,
                rejected: &never,
            };
            let typed = fx.us.render_all(keys).unwrap();
            match decide::boundary(&ctx, keys, &fx.us, Lang::En, false) {
                Verdict::Keep => *prev_kept = true,
                v => {
                    *fired.entry(format!("{typed} -> {v:?}")).or_default() += 1;
                    *prev_kept = false;
                }
            }
            keys.clear();
        };
        for ch in text.chars() {
            match fx.us.key_for(ch) {
                Some(k) if word_key(k.sc) => keys.push(k),
                _ => flush(&mut keys, &mut prev_kept, &mut total, &mut fired),
            }
        }
        flush(&mut keys, &mut prev_kept, &mut total, &mut fired);
    }
    let n: usize = fired.values().sum();
    println!("{total} tokens typed, {n} would be converted ({:.3}%)", 100.0 * n as f64 / total as f64);

    // What a keystroke costs: every word checked mid-word at each length and
    // then decided whole.
    let words = ["ghbdtn", "руддщ", "большими", "configuration", "привіт", "hello"];
    let started = std::time::Instant::now();
    let mut calls = 0;
    for _ in 0..2000 {
        for w in words {
            for lang in Lang::ALL {
                let Some(keys) = keys_of(&fx, lang, w) else { continue };
                let ctx = Context { models, targets: &targets, bias: 0.0, continues: false, rejected: &never };
                for n in 1..=keys.len() {
                    std::hint::black_box(decide::partial(&ctx, &keys[..n], &fx.us, Lang::En));
                    calls += 1;
                }
                std::hint::black_box(decide::boundary(&ctx, &keys, &fx.us, Lang::En, false));
                calls += 1;
            }
        }
    }
    println!("{:.1} µs per decision", started.elapsed().as_secs_f64() * 1e6 / calls as f64);
    let mut v: Vec<_> = fired.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, c) in v.iter().take(60) {
        println!("  {k} x{c}");
    }
}
