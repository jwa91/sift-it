# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com); versions follow
[SemVer](https://semver.org).

## [Unreleased]

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
