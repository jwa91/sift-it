# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com); versions follow
[SemVer](https://semver.org).

## [Unreleased]

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
- Quoted exact phrases: `"INV-2024-0113" invoice` only finds files with that exact text, ranked by the rest.
- `sift-it serve` previews for every kind of result: a picture, a PDF page, a video frame, a recording's waveform with
  the matched stretch highlighted, and a passage's whole text.
- Recordings that AVFoundation gives up on partway are read past the spot; files that couldn't be read get another
  try when the readers improve.
- `sift-it selftest` (built-in reference vectors), `sift-it eval`, `sift-it estimate`, `sift-it read`.
