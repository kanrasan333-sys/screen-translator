# Word models for layout correction

`en.bin`, `ru.bin`, `uk.bin` — per language, a frequency-ranked list of word
forms and a character trigram model. Read by `src/autotype/model.rs`, which
documents the format; rebuilt with

    python tools/build_punto_dicts.py

which downloads its sources into `tools/.cache/` on the first run
(about 160 MB) and takes under a minute after that. Add `--eval <dir>` to also
write the held-out word lists `cargo test --release eval -- --ignored` measures
accuracy on.

## Sources

- **OpenSubtitles 2018 word frequencies**, as published in
  [hermitdave/FrequencyWords](https://github.com/hermitdave/FrequencyWords) —
  content licensed [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).
  Derived from the OpenSubtitles corpus (P. Lison and J. Tiedemann, 2016,
  *OpenSubtitles2016: Extracting Large Parallel Corpora from Movie and TV
  Subtitles*, LREC 2016).
- **Wikipedia word frequencies**, as published in
  [IlyaSemenov/wikipedia-word-frequency](https://github.com/IlyaSemenov/wikipedia-word-frequency)
  (English, Russian and Ukrainian Wikipedia dumps of 2022–2023). Wikipedia
  text is licensed [CC BY-SA](https://creativecommons.org/licenses/by-sa/4.0/).

The files here are derived from those lists (filtered, merged, re-weighted and
truncated) and are distributed under the same **CC BY-SA 4.0** licence. The
rest of the repository keeps its own licence.
