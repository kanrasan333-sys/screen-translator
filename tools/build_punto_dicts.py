#!/usr/bin/env python3
"""Builds the word models behind the automatic layout switcher.

    python tools/build_punto_dicts.py [--cache DIR] [--out data/punto] [--eval DIR]

Writes data/punto/{en,ru,uk}.bin: a zlib stream holding a frequency-ranked word
list and a character trigram model for one language. The layout is documented
in src/autotype/model.rs, which is the only reader.

Sources, downloaded once into --cache:

  * OpenSubtitles 2018 word frequencies (github.com/hermitdave/FrequencyWords,
    content CC BY-SA 4.0). Conversational vocabulary: what people type in chats.
  * Wikipedia word frequencies (github.com/IlyaSemenov/wikipedia-word-frequency;
    Wikipedia text is CC BY-SA). Breadth, technical terms, and above all a clean
    Ukrainian reference.

The Ukrainian subtitle corpus is roughly a third Russian: "что", "меня" and
"нет" sit near the top of its list. Used as-is it teaches the model that those
are Ukrainian words, which is precisely the confusion the switcher must not
make. So the Russian share is measured on words spelled with ы/э/ъ/ё, letters
Ukrainian does not have, and the Russian subtitle frequencies scaled by that
share are subtracted from every other word. What survives is Ukrainian usage:
"так" and "тебе" keep most of their count, "что" and "меня" lose all of it.

Wikipedia cannot referee this directly. It is clean in both languages, but it
is written in a different register: colloquial Russian "про" is 170 times
rarer in the Russian encyclopedia than in the Ukrainian one, and weighing words
by that ratio deletes it from Russian altogether.
"""

import argparse
import collections
import math
import os
import random
import re
import struct
import sys
import urllib.request
import zlib

SUBS_URL = "https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2018/{lang}/{lang}_full.txt"
WIKI_BASE = "https://raw.githubusercontent.com/IlyaSemenov/wikipedia-word-frequency/master/results/"
WIKI_FILE = {
    "en": "enwiki-2023-04-13.txt",
    "ru": "ruwiki-2022-08-29.txt",
    "uk": "ukwiki-2022-08-30.txt",
}

# Symbol order: the sort order of the word list and the index into the trigram
# table, where 0 is the word boundary. Must match `ALPHABET_*` in model.rs.
ALPHABET = {
    "en": "abcdefghijklmnopqrstuvwxyz'",
    "ru": "абвгдеёжзийклмнопрстуфхцчшщъыьэюя",
    "uk": "абвгґдеєжзиіїйклмнопрстуфхцчшщьюя'",
}
LANG_ID = {"en": 0, "ru": 1, "uk": 2}

UK_LETTERS = "абвгґдеєжзиіїйклмнопрстуфхцчшщьюя"
WORD_RE = {
    "en": re.compile(r"^[a-z]+(?:'[a-z]+)*$"),
    "ru": re.compile(r"^[а-яё]+$"),
    "uk": re.compile(rf"^[{UK_LETTERS}]+(?:'[{UK_LETTERS}]+)*$"),
}

# Every apostrophe look-alike becomes the plain one the keyboard produces.
APOSTROPHES = str.maketrans({"’": "'", "ʼ": "'", "`": "'", "′": "'", "‘": "'"})

# How many words each dictionary keeps, and how many go into the trigram
# model (which wants breadth more than it wants exact frequencies).
DICT_SIZE = {"en": 200_000, "ru": 250_000, "uk": 250_000}
NGRAM_VOCAB = 1_000_000

# Words per block of the front-coded list: the unit of binary search.
BLOCK = 32

# Share of subtitles in the blend; the rest is Wikipedia.
SUBS_WEIGHT = 0.5

# Programming vocabulary that neither corpus ranks high enough, but that a
# developer types daily. Held at a modest floor frequency.
TECH_WORDS = """
aac angular ansible apache apk apt async avi awk aws azure babel backend bash bat
bitbucket blob bmp bool bootstrap brew bun bundler cargo cassandra cdn cfg changelog
chmod choco chown cli clojure cloudflare cobol compiler composer conda conf config
cors cpp crontab crud csharp csrf csv ctx cuda curl cypress dao dart ddos deb debugger
deno devops dhcp django dll dmg dns docker dockerfile docx dotnet dpkg dto
elasticsearch elixir emacs enum erlang eslint exe fastapi favicon ffmpeg flac flask
fn fortran frontend fsharp fullstack gcc gcp gem gif github gitignore gitlab golang
goroutine gpu gradle grafana graphql groovy grpc gta gui guid haskell heic helm
heroku htaccess htop html http https ico ide impl ini iso jar jenkins jest
journalctl jpeg jpg jquery json jsx julia jvm jwt kafka keras kotlin kubectl
kubernetes kvm lambda laravel linker localhost lua makefile malloc mariadb matlab
matplotlib maven middleware minify mkdir mkv mocha mongo mongodb monorepo mov mp3
mp4 mpeg mpg msi mutex mvc mysql namespace neovim nestjs netlify nextjs nginx nmap
nodejs nosql npm nuget numpy nuxt oauth objc ocaml ocr ogg opencv orm pacman pandas
pdf perl php pid pip pipenv playwright png pnpm polyfill postgres postgresql
powershell ppt pptx prettier printf println prometheus psd pwsh pytest pytorch
rabbitmq rails rar readme redis redux refactor refactoring regex repo rollup rpm
rsync ruby runtime rustc rustup saml sass scala scipy scoop scp scss sdk sed
selenium semaphore sftp sitemap sklearn soap sql sqlite ssd ssh sshd ssl sso
stacktrace stderr stdin stdout struct sudo svelte svg swift symfony sys systemctl
tailwind tar tcp telnet tensorflow terraform tgz tiff tls tmux toml transpile tsv
tsx tty txt udp url usb utf uuid vagrant varchar vec vercel vim vite vitest vlc
vmware vpn vscode wasm wav webassembly webhook webm webp webpack wget whl winget
xls xlsx xml xss yaml yarn yml zsh
""".split()
TECH_FLOOR = 1e-5

# Two-letter shorthand that chats and code are full of but the corpora barely
# know. Some read as Cyrillic words on the other layout ("cv" is "см"), so they
# need enough weight to be kept when typed. Not "db", "kb" or "vs": as "ви",
# "ли" and "мы" they are among the commonest words typed on the wrong layout,
# and in running English text the context keeps them anyway.
SHORT_WORDS = "ai am cd ci cv gb id io ip js mb ml os pc pm pr px qa ts ui ux vm".split()
SHORT_FLOOR = 5e-5

# Letters that are words on their own. Every other single letter in the
# corpora is an initial or a tokenizer's leftover — the "s" of "it's" — and
# must not pass for a word that protects ",s" from becoming "бы".
SINGLE_LETTER_WORDS = {
    "en": set("ai"),
    "ru": set("авиксоуяжб"),
    "uk": set("авійзоуяє"),
}
SINGLE_LETTER_CAP = 1e-6

# Two-letter words, the only two-letter strings allowed to count as words.
# The rest reach the corpora as subtitle OCR slips ("lf" for "If"), initials,
# abbreviations ("нб", "вб") or the other language spelled out ("мі"), and too
# many of them are another layout's common word — "lf" is "да", "dj" is "во",
# "рш" is "hi" — to be allowed to shield it, or to stand in for one.
TWO_LETTER_WORDS = {
    "en": set("""
        am an as at be by do go he hi if in is it me my no of oh ok on or so to up
        us we ah ha hm mm uh um yo ya aw ow eh ex mr ms dr st pm tv pc vs id ai ui
        os db kb ps""".split()),
    "ru": set("""
        ад ай ах бы во вы да де до ее её ей ем ею же за из им их ка ко ли мы на не
        ни но ну об ой ок он ор от ох по со та те то ты уж ух ща эй эх юг яд ёж ум
        ус ас ил рф чё че шо го хм мм ха эм ля фу уф пк ии вк тг""".split()),
    "uk": set("""
        ай ах аж би бо ви де до же за із їй їм їх її ім ін ми на не ні ну об ой ок
        ох по та те ти то ті ту це ці цю ця чи що як ще уж юг яд ум ус ас рф шо го
        хм мм ха пк ші тг""".split()),
}

# The same keys on the Russian and on the Ukrainian layout: the letters that
# differ, as a translation from one alphabet to the other.
RU_TO_UK = str.maketrans("ыэъё", "ієї'")
UK_TO_RU = str.maketrans("ієї'", "ыэъё")


def fetch(url, path):
    if os.path.exists(path) and os.path.getsize(path) > 0:
        return path
    print(f"  downloading {url}", file=sys.stderr)
    tmp = path + ".part"
    with urllib.request.urlopen(url, timeout=600) as r, open(tmp, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    os.replace(tmp, path)
    return path


def load_counts(path, lang):
    """word -> count, normalised and filtered to the language's alphabet.

    Hyphenated entries count towards each part: the switcher treats `-` as a
    word boundary, so "нибудь" has to be known on its own for "что-нибудь" to
    be left alone.
    """
    rx = WORD_RE[lang]
    counts = collections.Counter()
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            parts = line.split()
            if len(parts) != 2:
                continue
            tok, c = parts
            try:
                c = int(c)
            except ValueError:
                continue
            tok = tok.lower().translate(APOSTROPHES)
            pieces = tok.split("-")
            # "ин-т", "г-н": abbreviations, whose stubs are not words.
            if len(pieces) > 1 and any(len(x) < 2 for x in pieces):
                continue
            for piece in pieces:
                if len(piece) <= 32 and rx.match(piece):
                    counts[piece] += c
    return counts


def relative(counts):
    total = sum(counts.values()) or 1
    return {w: c / total for w, c in counts.items()}


RUSSIAN_ONLY = re.compile(r"[ыэъё]")
ANY_CYRILLIC = re.compile(r"^[а-яёєіїґ']+$")


def russian_mass(path):
    """(count of words spelled with ы/э/ъ/ё, count of all Cyrillic words)."""
    marked = total = 0
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            parts = line.split()
            if len(parts) != 2 or not parts[1].isdigit():
                continue
            tok = parts[0].lower()
            if not ANY_CYRILLIC.match(tok):
                continue
            c = int(parts[1])
            total += c
            if RUSSIAN_ONLY.search(tok):
                marked += c
    return marked, total


def remove_russian(uk_subs, ru_subs, uk_path, ru_path):
    """Subtracts the Russian mixed into the Ukrainian subtitles.

    If a fraction `c` of the Ukrainian corpus is really Russian, every word is
    expected to carry `c` times its Russian frequency in excess. `c` itself
    comes from the words only Russian can spell: their share of the Ukrainian
    corpus over their share of the Russian one. The margin on the subtraction
    errs towards removing a shared word's Ukrainian count rather than keeping
    a Russian word alive.
    """
    uk_marked, uk_total = russian_mass(uk_path)
    ru_marked, ru_total = russian_mass(ru_path)
    c = (uk_marked / uk_total) / (ru_marked / ru_total)
    print(f"  Ukrainian subtitles are {c:.0%} Russian", file=sys.stderr)
    for w in list(uk_subs):
        expected = 1.3 * c * uk_total * ru_subs.get(w, 0) / ru_total
        left = uk_subs[w] - expected
        if left <= 0:
            del uk_subs[w]
        else:
            uk_subs[w] = left


def remove_surzhyk(subs, wiki, sibling, translate):
    """Caps subtitle words that are the other language in this one's letters.

    Subtitles spell speech as heard: "єто" and "біло" in the Ukrainian ones,
    "тоды" in the Russian. Each is the *other* layout's reading of a common
    word — exactly the confusion to be undone — and as a dictionary entry it
    would protect the mistake. The encyclopedia does not write that way, so a
    word whose sibling image is far more common gets at most three times its
    Wikipedia frequency. Real words keep their weight ("ті" is as common in
    the Ukrainian Wikipedia as in the subtitles); spelled-out speech loses it.
    """
    s_total = sum(subs.values()) or 1
    w_rel = relative(wiki)
    own = blend(subs, wiki)
    capped = 0
    for w in list(subs):
        image = w.translate(translate)
        if image == w or sibling.get(image, 0.0) < 10 * own.get(w, 0.0):
            continue
        cap = 3 * w_rel.get(w, 0.0) * s_total
        if subs[w] > cap:
            capped += 1
            if cap <= 0:
                del subs[w]
            else:
                subs[w] = cap
    return capped


def blend(subs, wiki):
    ps, pw = relative(subs), relative(wiki)
    out = {}
    for w in ps.keys() | pw.keys():
        out[w] = SUBS_WEIGHT * ps.get(w, 0.0) + (1 - SUBS_WEIGHT) * pw.get(w, 0.0)
    return out


def encode(word, alphabet):
    return bytes(alphabet.index(ch) + 1 for ch in word)


def quantize(p):
    """-log2(p) in eighths of a bit, clamped to a byte."""
    return max(1, min(255, round(-math.log2(p) * 8)))


def train_trigrams(words, alphabet):
    """Witten-Bell smoothed P(c | a b) over the alphabet plus boundary (0).

    `words` is [(word, weight)]. Returns a K*K*K byte table of quantized
    -log2 probabilities, index (a*K + b)*K + c.
    """
    K = len(alphabet) + 1
    idx = {ch: i + 1 for i, ch in enumerate(alphabet)}
    c3 = collections.defaultdict(float)
    c2 = collections.defaultdict(float)
    c1 = [0.0] * K
    for w, wt in words:
        seq = [0, 0] + [idx[ch] for ch in w] + [0]
        for i in range(2, len(seq)):
            a, b, c = seq[i - 2], seq[i - 1], seq[i]
            c3[(a, b, c)] += wt
            c2[(b, c)] += wt
            c1[c] += wt

    total1 = sum(c1)
    p1 = [(c1[c] + 1.0) / (total1 + K) for c in range(K)]

    ctx2 = collections.defaultdict(float)
    types2 = collections.defaultdict(int)
    for (b, c), n in c2.items():
        ctx2[b] += n
        types2[b] += 1
    ctx3 = collections.defaultdict(float)
    types3 = collections.defaultdict(int)
    for (a, b, c), n in c3.items():
        ctx3[(a, b)] += n
        types3[(a, b)] += 1

    table = bytearray(K * K * K)
    for b in range(K):
        n2, t2 = ctx2.get(b, 0.0), types2.get(b, 0)
        p2 = []
        for c in range(K):
            if n2 > 0:
                p2.append((c2.get((b, c), 0.0) + t2 * p1[c]) / (n2 + t2))
            else:
                p2.append(p1[c])
        for a in range(K):
            n3, t3 = ctx3.get((a, b), 0.0), types3.get((a, b), 0)
            for c in range(K):
                if n3 > 0:
                    p = (c3.get((a, b, c), 0.0) + t3 * p2[c]) / (n3 + t3)
                else:
                    p = p2[c]
                table[(a * K + b) * K + c] = max(0, min(255, round(-math.log2(p) * 8)))
    return bytes(table)


def write_model(path, lang, ranked, ngram_words):
    alphabet = ALPHABET[lang]
    dictionary = ranked[: DICT_SIZE[lang]]
    oov_mass = max(1e-4, 1.0 - sum(p for _, p in dictionary))
    table = train_trigrams(ngram_words, alphabet)

    entries = sorted((encode(w, alphabet), quantize(p)) for w, p in dictionary)
    body = bytearray()
    prev = b""
    for i, (codes, q) in enumerate(entries):
        if len(codes) > 255:
            raise ValueError(codes)
        if i % BLOCK == 0:
            body += bytes([len(codes)]) + codes + bytes([q])
        else:
            lcp = 0
            while lcp < min(len(prev), len(codes)) and prev[lcp] == codes[lcp]:
                lcp += 1
            suffix = codes[lcp:]
            body += bytes([lcp, len(suffix)]) + suffix + bytes([q])
        prev = codes

    alpha_bytes = alphabet.encode("utf-8")
    header = b"PNT1" + struct.pack(
        "<BBH", LANG_ID[lang], len(alphabet), len(alpha_bytes)
    ) + alpha_bytes + struct.pack("<fBI", oov_mass, BLOCK, len(entries))
    raw = header + table + bytes(body)
    blob = zlib.compress(raw, 9)
    with open(path, "wb") as f:
        f.write(blob)
    print(
        f"  {lang}: {len(entries)} words, oov mass {oov_mass:.4f}, "
        f"{len(raw) / 1e6:.2f} MB raw, {len(blob) / 1e6:.2f} MB compressed",
        file=sys.stderr,
    )
    return dictionary


def ngram_training_set(dist):
    """Weights compress the frequency range: a character model should learn
    what words *look* like, and raw token counts would let a few hundred
    function words drown out the rest of the vocabulary."""
    top = sorted(dist.items(), key=lambda kv: -kv[1])[:NGRAM_VOCAB]
    floor = top[-1][1]
    weighted = [(w, math.log2(1.0 + p / floor)) for w, p in top]
    mean = sum(wt for _, wt in weighted) / len(weighted)
    return [(w, wt / mean) for w, wt in weighted]


# ---------------------------------------------------------------------------
# Evaluation material (not shipped): what the Rust evaluation test reads.
# ---------------------------------------------------------------------------

def write_eval(eval_dir, lang, ranked):
    os.makedirs(eval_dir, exist_ok=True)
    rng = random.Random(1234 + LANG_ID[lang])
    size = DICT_SIZE[lang]
    known = ranked[:size]
    words = [w for w, _ in known]
    weights = [p for _, p in known]

    # Typical typing: words drawn by frequency.
    typical = rng.choices(words, weights=weights, k=30000)
    # Real but rare words the dictionary does not hold.
    rare_pool = [w for w, _ in ranked[size : size + 300_000] if len(w) >= 3]
    rare = rng.sample(rare_pool, min(8000, len(rare_pool)))
    # Uniform over the dictionary: long-tail words the user may still type.
    uniform = rng.sample(words, 20000)

    for name, items in (("typical", typical), ("rare", rare), ("uniform", uniform)):
        with open(os.path.join(eval_dir, f"{lang}_{name}.txt"), "w", encoding="utf-8") as f:
            f.write("\n".join(items) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", default=os.path.join(os.path.dirname(__file__), ".cache"))
    ap.add_argument("--out", default=os.path.join(os.path.dirname(__file__), "..", "data", "punto"))
    ap.add_argument("--eval", default=None, help="also write evaluation word lists here")
    args = ap.parse_args()
    os.makedirs(args.cache, exist_ok=True)
    os.makedirs(args.out, exist_ok=True)

    subs, wiki, subs_path = {}, {}, {}
    for lang in ("en", "ru", "uk"):
        print(f"loading {lang}", file=sys.stderr)
        sp = fetch(SUBS_URL.format(lang=lang), os.path.join(args.cache, f"subs_{lang}.txt"))
        wp = fetch(WIKI_BASE + WIKI_FILE[lang], os.path.join(args.cache, f"wiki_{lang}.txt"))
        subs[lang] = load_counts(sp, lang)
        wiki[lang] = load_counts(wp, lang)
        subs_path[lang] = sp

    print("separating Russian from Ukrainian", file=sys.stderr)
    remove_russian(subs["uk"], subs["ru"], subs_path["uk"], subs_path["ru"])
    ru_dist = blend(subs["ru"], wiki["ru"])
    uk_dist = blend(subs["uk"], wiki["uk"])
    n = remove_surzhyk(subs["uk"], wiki["uk"], ru_dist, UK_TO_RU)
    m = remove_surzhyk(subs["ru"], wiki["ru"], uk_dist, RU_TO_UK)
    print(f"  capped {n} Ukrainian and {m} Russian spellings of the other language", file=sys.stderr)

    dists = {lang: blend(subs[lang], wiki[lang]) for lang in ("en", "ru", "uk")}
    for lang in ("en", "ru", "uk"):
        dist = dict(dists[lang])
        if lang == "en":
            for w in TECH_WORDS:
                if WORD_RE["en"].match(w):
                    dist[w] = max(dist.get(w, 0.0), TECH_FLOOR)
            for w in SHORT_WORDS:
                dist[w] = max(dist.get(w, 0.0), SHORT_FLOOR)
        for w in list(dist):
            if len(w) == 1 and w not in SINGLE_LETTER_WORDS[lang]:
                dist[w] = min(dist[w], SINGLE_LETTER_CAP)
        # Two letters make a word only if they are on the list: the rest of
        # them in the corpora are abbreviations ("нб", "вб") and noise, and
        # would otherwise stand in for real words on the other layouts.
        for w in list(dist):
            if len(w) == 2 and w not in TWO_LETTER_WORDS[lang] and w not in SHORT_WORDS:
                dist[w] = min(dist[w], SINGLE_LETTER_CAP)
        ranked = sorted(dist.items(), key=lambda kv: (-kv[1], kv[0]))
        write_model(os.path.join(args.out, f"{lang}.bin"), lang, ranked, ngram_training_set(dist))
        if args.eval:
            write_eval(args.eval, lang, ranked)


if __name__ == "__main__":
    main()
