# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com); versions follow
[SemVer](https://semver.org).

## [Unreleased]

### Added

- The engine: one resident process (`sift-it engine`, a LaunchAgent beside the indexer's) answers every search, from
  the terminal, scripts and front ends, over a Unix socket in the data folder. `sift-it serve` relays to it and makes
  previews itself; `sift-it search` and the finder ask it directly. A client of another build replaces it. A data
  folder too deep for a socket's name is reached through a short link in the user's temporary folder.
- Searches by `words` or by `meaning`, per search (`--by`, or `mode` in a request) with a default in `search.mode`.
  By words loads nothing. The first search by meaning loads the model and the vectors, and they stay until
  `sift-it engine unload`.
- `sift-it status` shows what the engine holds, and how the indexer uses its memory.
- `indexing.memory_mb` decides how fast indexing goes, never what it writes: how many files are read at once, and
  whether what's said in recordings is written down beside the reading or after it. At `auto` it's a quarter of the
  Mac's memory: on 8 GB the embedding and speech models take turns, on 24 GB they run together.
- `search.vectors` sets which form of the vectors a search scans: `exact-f32`, `exact-f16`, which reads half as much,
  or `sketch`, which compares the query with a 96-byte sketch of every vector and scores the closest few thousand
  exactly. `auto`, the default, scans the most precise form whose rows fit in `search.memory_mb`, a tenth of the Mac's
  memory unless set. `sift-it bench-search` measures all three, and `sift-it eval --compare` how much of the exact top
  20 a search with sketches keeps.

### Changed

- The vectors are in files beside the index (`vectors/`), each written in three forms: as made, at half precision, and
  as a sketch. The engine maps them instead of copying them into memory, so macOS can drop their
  pages under pressure. Code has a partition of its own. When a quarter of a partition belongs to removed files, the
  indexer writes it again while it's idle.
- A query's median, which the lift is measured from, comes from a fixed sample of 8,192 vectors kept in memory, so it's
  the same whichever form a search scans.
- One `config.json` with a section for each part of the work: `sources`, `reading`, `ranking`, `indexing` and `log`.
  Every value is written out, `sift-it config <key> <value>` sets any of them by its dotted name, and a value the file
  leaves out keeps its default.
- Reading values (depth, token budgets, passage sizes, video sampling, a video's sound) resolve per file: the values
  for everything, then the kind, the file type, and rules for paths. A change reads again only the files it affects.
- `reading.depth` decides how deep a file is read: `words` (its name, tags and words, no model), `meaning` (also
  embedded) or `speech` (also what's said, written down; the default). Each builds on the one before.
- Every file is read in full: there are no size, page or length limits. The queue goes from the cheapest file to the
  dearest, by an estimate made when a file is queued, and `sift-it status` shows what's waiting and the files that take
  longest.
- Text files, code and documents are read as a stream, and a big file's segments are written in parts as they come:
  memory doesn't grow with a file, and a search finds the part that's in.
- The model loads when a file first needs a vector, so indexing at `words` depth never loads it.
- `sift-it read` recognizes screenshots as the indexer does.
- `sift-it service install` installs the engine's LaunchAgent with the indexer's.
- serve's events: `ready` says whether search by meaning is loaded, and `meaning` (loading, ready, unavailable,
  unloaded) replaces `encoder`.
- Kinds are plaintext, document (PDFs included), image, screenshot, audio, video and code: `--kind pdf` becomes
  `--kind document`.
- `sources.ignore` takes patterns for files as well as folders, such as `*.psd` or `**/Exports/**`.
  `sift-it folders exclude` adds one.
- The constants search ranks by move into `ranking`, with the same values.

### Removed

- The options `include-code`, `index-on-battery`, `transcribe` and `exclude-ext`: `sources.code`,
  `indexing.on_battery`, `reading.depth` and `sources.ignore` replace them.
- The limits on file size, PDF pages and recording length.
- `sift-it search --words-only`: `--by words` replaces it.

### Fixed

- A file can no longer stall indexing for good. Vision sometimes never returns from reading the text on a picture,
  right after it compiles its models for the Neural Engine (once per binary), and nothing in the process can stop it.
  A reader that hands over nothing for ten minutes is now stuck: its file is set aside and tried again six hours
  later, and the indexer makes way for a new process (launchd starts the daemon again; `sift-it index` says to run it
  again).
- A recording or video the indexer stopped writing down right after its last stretch is no longer marked as failed:
  it went on from its very end, which AVFoundation can't read. After its last stretch a file is written down.

## [0.2.0] — 2026-10-16

### Added

- What's said in recordings and videos is written down in the background, once everything else is indexed: passages
  of a sentence or a few, with their time, found by meaning and by their words. Parakeet Ultra on transcribe.cpp, in a
  helper process (`sift-it-transcribe`) beside `sift-it`, a few minutes of sound at a time; changes in the folders come
  first. `sift-it status` shows how far it is, `sift-it config transcribe off` turns it off.
- The tags of recordings and videos (title, artist, album) are found by their words.
- `sift-it model download` also fetches the speech model (740 MB), unless Handy has the same file.

### Changed

- The index moves to version 3 in place: nothing indexed so far is read again. An older sift-it can't read it after.

### Removed

- `scripts/make-demo.sh`: the demo collection and its script live outside the repository, in the workspace's
  `sift-it-data` folder (see docs/ranking.md).

## [0.1.3] — 2026-10-14

### Changed

- Explain the DigUp relationship, ranking changes, terminal interface, and Raycast integration in the README.
- Preserve Abdur Rahim's MIT notice with the reference test files and in the project license.
- Describe terminal and Raycast use in the generated Homebrew cask.

## [0.1.2] — 2026-10-14

### Fixed

- The finder starts each new search at its first result. It kept the file that was selected, wherever that landed in
  the new list (often a file it had kept from a half-typed word).
- A match in a text or code file opens at its line when paragraphs in the file are more than one blank line apart
  (it opened that many lines too early). Text and code files are read again once, to fix what's indexed.
- A file renamed while it was being read is no longer marked unreadable under its new name.
- Searches that leave kinds out, and words-only searches, no longer miss files when a long file or files of another
  kind hold the words many times.
- A chosen folder that can't be read for a moment (an unplugged drive) keeps its files in the index, also when a
  change inside it comes in.
- A folder replaced by a symlink is not followed when a change in it comes in, as a full crawl doesn't.

## [0.1.1] — 2026-10-14

### Changed

- Ranking on a real mix of files: a long file's lift is lowered by about what its length alone explains, so a book's
  best passage no longer outranks the recording, photo or video a query is about (on the demo folder with eight books,
  a book came first for 21 of 95 queries about something else, now 1). Ordinary words get a small bonus instead of
  the full one, which stays for numbers, codes, quoted words and `code:` searches. See docs/ranking.md.

### Fixed

- `sift-it service install` over a running indexer: it now waits for the old one to stop before starting the new one,
  instead of failing with launchctl's "Bootstrap failed: 5: Input/output error".

## [0.1.0] — 2026-10-14

### Added

- Search files by what's in them with EmbeddingGemma 2 (Q8_0) on llama.cpp b11461 with Metal: one vector space for
  text, pictures and sound.
- Readers for text and Markdown, rich documents (textutil), PDFs (PDFKit, with OCR for scanned pages), pictures in
  every format macOS opens (ImageIO), screenshots (picture + Vision OCR), recordings (30 s windows) and video (one
  frame per shot, plus its sound), and source code and notebooks (optional).
- Hybrid ranking: cosine similarity gated on its lift above the query's median, plus exact words (SQLite FTS5);
  results open at their page, moment or line.
- `sift-it daemon`: background indexing with FSEvents, new files first; `sift-it service` installs it as a LaunchAgent.
- `sift-it serve`: a JSON-lines search server for front ends (search, recent files, previews).
- `sift-it` (or `sift-it find`): search as you type, full screen, with a preview per kind of file (pictures, PDF
  pages and video frames inline in terminals that show pictures, a recording's waveform, highlighted passages and
  code); ↩ opens a result where it matched.
- `sift-it search` made for the terminal: numbered results, file names as links, where each matched, and a body per
  kind (marked passages, numbered code, a waveform in block characters, pictures inline); paths only when piped.
- `sift-it open <n>`: the last search's nth result, where it matched (QuickTime at the moment, Preview at the page,
  `$EDITOR` at the line).
- Quoted exact phrases: `"INV-2024-0113" invoice` only finds files with that exact text, ranked by the rest.
- `sift-it serve` previews for every kind of result: a picture, a PDF page, a video frame, a recording's waveform with
  the matched stretch highlighted, and a passage's whole text.
- Recordings that AVFoundation gives up on partway are read past the spot; files that couldn't be read get another
  try when the readers improve.
- `sift-it selftest` (built-in reference vectors), `sift-it eval`, `sift-it estimate`, `sift-it read`.
