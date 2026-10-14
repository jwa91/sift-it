# sift-it

Search the files on your Mac by what's in them. Describe what you're looking for and sift-it finds it, whatever kind of
file it is: a clause on page 3 of a PDF, a dog in a photo, the minute of a podcast where they talk about sleep, the
error in a screenshot, a function in your code. Everything runs on your Mac, with Google DeepMind's
[EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), a model that puts text, pictures and sound in one
shared space.

It's one command, `sift-it`: point it at your folders, let it index in the background, and search from the terminal.
Other programs can build on it too: `sift-it serve` answers searches as JSON, and most commands take `--json`.

## Install

Apple Silicon, macOS 14 or later.

```sh
brew install jwa91/tap/sift-it
sift-it model download                               # the model, 865 MB, once
sift-it folders add ~/Documents ~/Desktop ~/Downloads
sift-it service install                              # index in the background, from login on
sift-it search "the clause about pets in the lease"
```

macOS asks before a program reads Documents, Desktop or Downloads; allow sift-it there (or give it Full Disk Access in
System Settings → Privacy & Security). `sift-it selftest` checks that the installed binary makes the reference vectors.

## Using it

Run `sift-it` on its own to search as you type, full screen: results on the left, and on the right a preview of the
selected one that fits its kind. Pictures, PDF pages and video frames show inline (in Ghostty, kitty, iTerm2 and
WezTerm), recordings as a waveform with the stretch that matched lit up, and passages and code with your words
highlighted. ↩ opens it where it matched, ^R shows it in Finder, ^Y copies its path, Tab narrows to documents,
pictures, recordings, videos or code, and Esc leaves.

```
sift-it ❯ the clause about pets                                                       everything
┌ 1 match ──────────────────────┐┌ agreement.pdf · p. 3 ──────────────────────────────────────┐
│▶ agreement.pdf           p. 3 ││ [page 3, rendered]                                          │
│                               ││                                                            │
│                               ││ 7. Animals. The tenant may keep one cat or one small dog   │
│                               ││ only with the landlord's written consent. A pet deposit of │
│                               ││ EUR 300 applies…                                           │
│                               ││ ~/Documents/Housing · 4 pages · 23 KB                       │
└───────────────────────────────┘└────────────────────────────────────────────────────────────┘
 ↩ open at page 3   ^R show in Finder   ^Y copy path   tab kind   esc quit
```

Or search in one go: each result says where it matched and shows what fits its kind:

```
$ sift-it search "where they talk about sleep"

 1  episode-12.m4a · 0:20–0:50                                         ~/Podcasts
    ▇▇█▁▁▆▅▆▁█▅▁▃▆▄▆█▆▇▅▄▇▅▂▄▆▃▂▃▆▃▆▅▆▃▅▂▁▄▄▇▆▃▁▆▆▅▁▂▆▇▁▅   heard at 0:20–0:50 of 1:16

 2  notes-0311.md · line 3                                             ~/Documents
    Read a long piece about sleep this week. The main points worth keeping: keep the same
    wake-up time every day, weekends included. The body clock anchors on the morning…

↳ sift-it open <n> opens a result where it matched; --reveal shows it in Finder

$ sift-it open 1          # QuickTime, playing from 0:20
```

Passages show with your words highlighted, code as numbered lines, and recordings as a waveform with the stretch that
matched lit up. In Ghostty, kitty, iTerm2 and WezTerm, pictures and video frames show inline, and file names are links.
Piped, it prints paths, one per line: `sift-it search "invoices from March" | xargs open`.

`sift-it open <n>` opens a result where it matched: a recording or video at its moment (QuickTime), a PDF at its page
(Preview), text and code at the line in `$EDITOR` (vim, nvim, helix, emacs and nano right in the terminal; VS Code,
Cursor, Zed and Sublime Text at the line).

```
sift-it [find [<words>]]               search as you type, full screen
sift-it search <words> [--kind pdf,image] [-n 10] [--json]
sift-it search "code: retry a failed call with backoff"    code only (needs `sift-it config include-code true`)
sift-it search '"INV-2024-0113" invoice'                   quoted words must be there exactly
sift-it open <n> [--reveal]                                open a result of the last search where it matched
sift-it open <file> [--at <page|second|line>]
sift-it folders [add|remove|exclude|include] <folder>...   the folders to search
sift-it estimate [<folder>...]                             files, and about how long indexing them takes
sift-it config [include-code|index-on-battery|exclude-ext] <value>
sift-it index [--reindex]              bring the index up to date, then exit (asks the daemon when it runs)
sift-it daemon                         keep it up to date in the background
sift-it service install|uninstall|status|plist   the daemon as a LaunchAgent
sift-it status [--json]
sift-it model status|download
sift-it read <file> [--tokens]         what sift-it reads from a file
sift-it serve                          a search server on stdin/stdout (JSON lines), for front ends (src/serve.rs)
sift-it selftest                       the reference vectors
sift-it eval evals/evals.json          search quality on the testbed
```

Exact words count too: invoice numbers, error codes and names are matched as written, in file names, document text
and the text in screenshots. `NO_COLOR` turns colour off, `SIFT_IT_IMAGES=0` inline pictures.

Its files live in `~/Library/Application Support/Sift It` (or `$SIFT_IT_HOME`): `config.json`, `index.sqlite`,
`status.json`, `indexer.log`, `models/` and `previews/`. `brew uninstall --zap sift-it` removes all of it.

## What it reads

Only the folders you choose, and only reads them: it never changes, moves or uploads a file.

- **Documents**: txt, md, rtf, doc, docx, odt, html (macOS's `textutil`), in passages of about 1,200 characters.
  Text that's mostly numbers is matched by its exact words only.
- **PDFs**: page by page (PDFKit). Scanned pages are read by Apple's text recognition (Vision); a scanned page with
  no text is embedded as a picture.
- **Pictures**: every format macOS can open, HEIC and RAW included (ImageIO), at 140 tokens.
- **Screenshots**: as pictures (280 tokens), and the text on them is read too.
- **Recordings**: any length, in 30-second windows that overlap by 10 seconds; a match points to its moment. Silence
  is skipped.
- **Videos**: one frame per shot (and every 30 seconds within a long one), plus the sound like a recording.
- **Code** (optional): source files and notebooks, in stretches of about 600 characters, titled with their path in
  the project. Only `code:` searches show code.

It skips hidden folders, `~/Library` (below a chosen folder), app bundles and packages, dependency and build folders,
keys and other secrets, and iCloud files that are only in the cloud. Inside a code project it reads only pictures,
PDFs and recordings, unless code reading is on.

## How it works

```
your folders ─► readers (textutil, PDFKit, ImageIO, Vision, AVFoundation — through objc2)
             ─► EmbeddingGemma 2 (Q8_0) on llama.cpp with Metal: text directly, pictures and sound through
                the model's own encoders (mtmd), each input one 768-dimensional vector
             ─► one SQLite file: files, segments with their vectors, and an FTS5 index of their words
your words   ─► the same model's text part ─► every vector scored (one Accelerate matrix-vector product)
                + the FTS5 index for exact words ─► one result per file, at its best page, moment or line
```

**Ranking.** A segment's cosine similarity with the query ranks it. Whether it's shown depends on its *lift*, how far
it stands above the median segment for this query (in a small index, a median pulled toward a typical one): queries
differ in how close they land to everything, and the lift takes that out. A file counts by its best segment, and the
best of a book's hundreds of passages fits nearly any query a little, so a long file's lift is lowered by about what
its length alone explains. A strong match lifts at least 0.13, a weaker one 0.09, and below that nothing is shown, so a
search with no answer says so. A segment with all of the query's words gets a bonus: a large one when one of them is a
number or a code, because no vector holds an invoice number exactly, and a small one for ordinary words, which the
model has read already (a note that mentions steam and a train doesn't outrank a photo of a steam train). See
[docs/ranking.md](docs/ranking.md).

**Indexing.** Readers run on a few worker threads and stream their pieces to the one thread that owns the model. Each
worker has its own small channel and the model thread takes from them in turn, so a long video can't hold up a short
note. A file is written in one transaction once all of it is in, and only if it hasn't changed meanwhile. The daemon
watches the folders with FSEvents, handles each changed path a moment after it goes quiet, and lets the model go after
a minute with nothing to do. It can wait for a charger, and always waits in Low Power Mode.

**Correctness.** `sift-it selftest` embeds fixed texts, a screenshot, a photo and a speech clip, and checks the vectors
against reference vectors from a known-good pipeline (cosine ≥ 0.999). The index records which pipeline made its
vectors; a different model or llama.cpp release re-embeds everything, and until it has, search uses exact words only.

## Measured

On an M5 with 24 GB:

| | |
|---|---|
| Text | about 10,000 tokens/s (50 passages of prose a second) |
| A photo / a screenshot | 140 ms / 360 ms (plus OCR, in parallel) |
| 30 s of sound | 250 ms: an hour-long recording in about 50 s |
| 514 mixed files (300 photos, 100 ten-page PDFs, 30 screenshots, notes, 20 min of recordings and video) | 3 minutes |
| A search over 200,000 vectors | 9 ms (after 0.8 s to load them) |
| A new file, searchable | 2 s after it lands, even while a long video is being indexed |

On the testbed (`make testbed`: 39 files of every kind, with names that give nothing away), all 43 queries in
`evals/evals.json` put the right file first, and all 9 that check the place land on the right page, moment, line or
cell. On the demo folder with eight books (`docs/demo-queries.json`), 82 of 107 queries put the right file first, and
a book comes first for 1 of the 95 that aren't about a book.

## Build from source

You'll need Xcode's command line tools, Rust and CMake (`brew install cmake`).

```sh
git clone https://github.com/jwa91/sift-it && cd sift-it
make build          # llama.cpp at a pinned release (a minute or two, once), then sift-it
make check          # fmt, clippy, tests
make selftest       # the reference vectors (needs the model: target/release/sift-it model download)
make eval           # search quality on the testbed
make install        # → ~/.local/bin/sift-it
```

For a demo, `scripts/make-demo.sh` fills `~/sift-it-demo` with about 370 MB of public photos, sounds, recordings,
music, videos, PDFs, books and notes. The files have camera-roll and Downloads-folder names, and `CREDITS.csv` says
where each one came from.

Releases: `make release VERSION=x.y.z`; [docs/releasing.md](docs/releasing.md) sets a Mac up for it.

## Credits

[EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2) by Google DeepMind (Apache 2.0), in
[ggml-org's GGUF conversion](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF);
[llama.cpp](https://github.com/ggml-org/llama.cpp) (MIT). The reference vectors in `tests/reference` and the idea come
from [DigUp](https://github.com/ARahim3/DigUp) (MIT). Gemma is a trademark of Google LLC.
