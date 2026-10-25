# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com); versions follow
[SemVer](https://semver.org).

## [0.4.3] — 2026-10-25

### Added

- `sift-it mcp`: an MCP server over stdio, for agents such as Claude Code, Claude Desktop and Codex. Two read-only
  tools: `search`, with each result's place, excerpt and, for a conversation, the command that resumes it; and `read`,
  a result's passage with the ones around it (a conversation's neighbouring exchanges), or an indexed file's text.
  It's a client of the engine like the app, so it loads no model itself. It answers the current MCP revision and the
  `initialize` handshake of earlier ones.
- A skill for agents with a shell (`skills/sift-it/SKILL.md`), and the README's Agents section on setting up both.
- `around` in the protocol's `text` request: the segments before and after the one asked for.

## [0.4.2] — 2026-10-25

Conversations with agents become searchable, iCloud Drive's Optimize Mac Storage no longer costs what's indexed, and
the docs say what sift-it is today.

### Added

- Claude Code's and Codex's session logs are a kind of their own, `conversation`, in any chosen folder
  (`~/.claude/projects`, `~/.codex/sessions`, even though they're hidden). What the user said and what the agent
  answered is read, as exchanges (a new segment, `exchange`, placed by when it was said); tool calls and their output,
  thinking and subagents' transcripts aren't. A result is named by the conversation's title, and carries the folder it
  started in and the command that resumes it, which `sift-it open` runs. A log still being written is read on from
  where it ended.
- `vectors_by_kind` in status: the vectors of each kind's files, beside `by_kind`.

### Changed

- iCloud Drive with Optimize Mac Storage: a file only in the cloud is found by its name, with nothing of it read, and
  it's read once it's downloaded. A file macOS removes the download of stays in the index as it was, and nothing of it
  is read again when it's back; work it still had waits for its download. sift-it never downloads a file: reading one
  that's only in the cloud fails instead, previews included. `cloud` in status counts the files waiting for their
  download, and `sift-it estimate` counts them apart (`cloud` in its JSON).
- Audio, not recordings: the CLI, the terminal search and its filter say "audio", as the app does.
- The README and docs say what sift-it is and does now, without comparisons to earlier versions of itself.

## [0.4.1] — 2026-10-25

### Changed

- On battery at `indexing.on_battery: words` (the default), and in Low Power Mode, the OCR round goes on with the words
  round: neither needs a model, and together they make every file findable by its exact words, the text on screenshots
  and scans included. The rounds that need a model still wait for a charger.

## [0.4.0] — 2026-10-25

Indexing in rounds: every file is found by its words within seconds of choosing the folders, and its meaning follows a
round at a time. What's said is written down on the Neural Engine, the models are shared in the Hugging Face cache, and
indexing takes less memory and less time throughout.

### Changed

- Where speech is heard in a recording or a video, the sound round embeds a window every two minutes instead of every
  20 s: what's said is found by its passages. A file's speech comes before its sound for that. Music, nature and the
  stretches between what's said keep every window, and sung lyrics written down don't count as speech (they have
  fewer words a second). Three podcasts index in 33 s, with 56 sound windows instead of 327. Recordings and videos
  are read again once.
- `sift-it estimate` says what each round will have to do in a folder and about how long, at the depth its files are
  read: `rounds` in its JSON (`round`, `total` in the round's unit, `unit`, `seconds`), in place of `speech_seconds`.
  Its `seconds` is how long all of it takes with the rounds side by side, as the plan at `indexing.memory_mb` has
  them. On the demo folder it says 2,027 s, and indexing took 2,037 to 2,109 s.
- Indexing goes in rounds over the whole collection, each with one tool: words first (names, tags, a PDF's text
  layer, documents through textutil: no model), then what text means, the text on screenshots and scans, what
  pictures show, what sound sounds like, and what's said. Every file is found by its exact words within minutes of
  choosing the folders, and meaning comes round by round. On the demo folder (628 files, 28 hours of sound), every
  file is found by its words after 5 or 6 s instead of 35 minutes, and its texts by meaning after 93 to 104 s; it's
  all done as soon as before, or 28% sooner at 2048 MB, and the indexer peaks at 756 to 822 MB instead of 1,018 to
  1,107 MB.
- `indexing.memory_mb` is a ceiling: each round's tool says its chip and what it holds, and the rounds that fit run
  side by side, one per chip (the GPU, the Neural Engine), one tool at a time on a small budget. When macOS says memory
  is short, indexing shrinks to one tool; when it's critical, it stops and lets everything go until it isn't.
- New and changed files take a fast lane: they go first in every round they need, so a new screenshot is found by its
  words and what it shows in seconds, even while a long video is read.
- Long recordings and videos are read five minutes at a time, and a stopped file goes on from where it was.
- `indexing.on_battery` is `words` (the default: words and new files go on, the rest waits for a charger), `all` or
  `none`, in place of `true` and `false`. Low Power Mode is `words` at most, where it waited. A config.json that says
  `true` or `false` there can't be read: `sift-it config indexing.on_battery words` sets it right.
- A config.json that can't be read names the key and what it takes. `sift-it config <key> <value>` sets it right, and
  the indexer waits for it, with its status saying why, where it exited before.
- The indexer's status (`status.json`, the protocol's IndexerStatus) says how far each round is: `rounds`, with each
  round's state, what's done of its total in its own unit, and the time it has left at the speed it went. `done`,
  `total`, `speech_done`, `speech_total` and the `transcribing` state are gone. `sift-it status` shows the rounds.
- The index is made again (a new schema).
- What's said in recordings and videos is written down on the Neural Engine: Parakeet Ultra through
  [FluidAudio](https://github.com/FluidInference/FluidAudio) (Core ML), in place of transcribe.cpp on the GPU. It's
  3.5 times as fast in the indexer (about 220 times as fast as the sound plays, was 63), takes 4.8 times less energy,
  and leaves the GPU to the embedding model: the reading beside it takes 7% longer than alone, where it took 30% longer
  beside transcribe.cpp. Three podcasts and four videos index in 138 s instead of 224 s. Its model is FluidInference's
  Core ML conversion (632 MB, was a 740 MB GGUF). The helper holds about 110 MB of its own (was 315 MB), and the Neural
  Engine 607 MB more for its copy of the model. Every recording is written down again once.
- What's said is cut into passages where sentences end (the helper hands over each word with its time), not in
  windows of about 28 seconds: 92% of passages end with a sentence, where half did, and a result opens closer to its
  moment.
- A new embedding model (or `sift-it index --reindex`) embeds what's written down of recordings again, without writing
  it down again.
- Building sift-it takes Xcode 26 (Swift 6.2): the speech helper is Swift now.
- The model files are kept in the Hugging Face cache (`~/.cache/huggingface/hub`, or `$HF_HUB_CACHE`, or
  `$HF_HOME/hub`), which other apps share. A model file is found by its hash wherever in the cache it is, so a copy
  another app downloaded isn't downloaded again; the speech model, a folder of files, in its own repository's folder.
  sift-it only adds what's missing to the cache, and never removes anything from it. The models in `~/Library/Application Support/Sift It/models` aren't used any more: `sift-it model download`
  fetches them into the cache, and that folder can go. `sift-it model status` says which cache (`cache`, in place of
  `folder` and `speech_model`).
- The indexer loads the vision encoder only while it reads pictures and video frames, and the audio encoder only while
  it reads sound, and lets them go when the reading is over: text alone, and writing down what's said after the
  reading, take the text model only. Reading text takes 336 MB instead of 910 MB, photos 544 MB and music 601 MB
  instead of about 870 MB, and writing down speech after the reading 475 MB with the helper (and 607 MB on the Neural
  Engine) instead of 1.8 GB.
- llama.cpp b11856 (was b11461): passages embed 8% faster and sound 9% faster, a query about 1 ms slower. Pictures'
  vectors move a little (cosine 0.9989 or more to the old ones), so an existing index is read again once. After that, a
  llama.cpp release that passes `sift-it selftest` keeps the index as it is: the index records the model and the
  pipeline's version, no longer the llama.cpp release.
- The engine's model has a context made for one query: 32 MB of compute buffers instead of 63 MB.
- ggml's Metal residency sets are off. They kept about 330 MB wired for three minutes after every search, and a thread
  woke 174 times a second for as long as the engine ran, idle or not. Embedding is as fast without them.
- `sift-it folders` (and `folders add`, `remove`, `include`, `exclude`) lists the folders, one a line, where it printed
  all of config.json.
- The release archive carries THIRD_PARTY_NOTICES: the licenses of llama.cpp, FluidAudio and the Rust crates built
  into `sift-it` and `sift-it-transcribe`.

### Removed

- `SIFT_IT_MODELS` and `SIFT_IT_SPEECH_MODEL`: `HF_HUB_CACHE` or `HF_HOME` say where the models are.
- transcribe.cpp, and the speech model's GGUF (`handy-computer/parakeet-ultra-gguf`): sift-it doesn't download it any
  more, and Handy's copy no longer serves it. sift-it leaves it in the Hugging Face cache, for the apps that use it.

## [0.3.1] — 2026-10-19

### Changed

- The finder (`sift-it` without arguments) lists nothing until something is typed, instead of the recently changed
  files.
- `sift-it status` says how much of the folders is indexed, in percent, while files wait.
- A change to `search`, `ranking` or `log` in config.json no longer stops the indexer partway: only `sources`,
  `reading` and `indexing` change its work.
- `sift-it estimate` counts writing down what's said (`speech_seconds` in its JSON, and in `seconds`), and a video's
  frames as often as 0.3.0 keeps them: on the demo folder it said 26 minutes for an hour's work. `sift-it status`
  says how long the speech still to write down takes.

### Fixed

- `sift-it config reading.video.keep_every_s null` (a kind's or a file type's value put back) said "there's no
  reading.video.keep_every_s" and failed, though it had done it.

## [0.3.0] — 2026-10-19

### Added

- The engine: one resident process (`sift-it engine`, a LaunchAgent beside the indexer's) answers every search, from
  the terminal, scripts and front ends, over a Unix socket in the data folder. `sift-it serve` relays to it and makes
  previews itself; `sift-it search` and the finder ask it directly. A client of another build replaces it. A data
  folder too deep for a socket's name is reached through a short link in the user's temporary folder.
- Searches by `words` or by `meaning`, per search (`--by`, or `mode` in a request) with a default in `search.mode`.
  By words loads nothing. The first search by meaning loads the model and the vectors, and they stay until
  `sift-it engine unload`.
- `sift-it status` shows what the engine holds, and how the indexer uses its memory.
- The protocol front ends speak has a crate of its own (`sift-it-protocol`) and a description (`docs/protocol.md`).
  Through it the engine also changes the settings (`config`) and the folders (`folders`), reports the indexer's
  progress in `status`, and says when it changes (an `indexer` event). A front end needs nothing else.
- `indexing.memory_mb` decides how fast indexing goes, never what it writes: how many files are read at once, and
  whether what's said in recordings is written down beside the reading or after it. At `auto` it's a quarter of the
  Mac's memory: with room for both, the embedding and speech models run together, and otherwise they take turns.
- `search.vectors` sets which form of the vectors a search scans: `exact-f32`, `exact-f16`, which reads half as much,
  or `sketch`, which compares the query with a 96-byte sketch of every vector and scores the closest few thousand
  exactly. `auto`, the default, scans the most precise form whose rows fit in `search.memory_mb`, a tenth of the Mac's
  memory unless set. `sift-it bench-search` measures all three, and `sift-it eval --compare` how much of the exact top
  20 a search with sketches keeps.
- A kind for records, `structured`: csv, tsv, json, jsonl, yaml and toml files. CSV and TSV are read row by row with
  their header row, JSON by its records or keys (minified too), JSON Lines by its lines, YAML by its keys and TOML by
  its tables. Lock files aren't read, and in a code project records are read only with the code.

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
- Kinds are plaintext, document (PDFs included), structured, image, screenshot, audio, video and code: `--kind pdf`
  becomes `--kind document`.
- `sources.ignore` takes patterns for files as well as folders, such as `*.psd` or `**/Exports/**`.
  `sift-it folders exclude` adds one.
- The constants search ranks by move into `ranking`, with the same values.
- Text is cut by its form: Markdown and plain text at their headings as well as their paragraphs, code at its
  definitions (found by indentation, with the comments and attributes above them). A part too long to be whole is cut
  at its next weaker breaks, and its pieces are embedded with its first line: a heading, a signature, a key. A long
  paragraph is cut into pieces of about the passage size. Notebook cells are cut as code or as Markdown.
- A text or code result opens at the line with the most of the query's words.
- serve's `preview` refuses a `kind` it doesn't know, instead of drawing the file as a picture.
- The code is laid out in parts with one job each (docs/architecture.md): the query path, the index, the ingest path,
  the plans for memory, and the embedding model in a crate of its own (`sift-it-llama`).

### Removed

- The options `include-code`, `index-on-battery`, `transcribe` and `exclude-ext`: `sources.code`,
  `indexing.on_battery`, `reading.depth` and `sources.ignore` replace them.
- The limits on file size, PDF pages and recording length, and on how many frames a video gives: a long video is
  looked at every two seconds too.
- `sift-it search --words-only`: `--by words` replaces it.

### Fixed

- A file can no longer stall indexing for good. Vision sometimes never returns from reading the text on a picture,
  right after it compiles its models for the Neural Engine (once per binary), and nothing in the process can stop it.
  A reader that hands over nothing for ten minutes is now stuck: its file is set aside and tried again six hours
  later, and the indexer makes way for a new process (launchd starts the daemon again; `sift-it index` says to run it
  again).
- A recording or video the indexer stopped writing down right after its last stretch is no longer marked as failed:
  it went on from its very end, which AVFoundation can't read. After its last stretch a file is written down.
- Sound decodes the same on every read. AVFoundation's own resampling and its decoders' last buffers varied from read to
  read; sound is now decoded at its own rate to the end of its tracks and resampled by one converter
  (docs/audio-decoding.md). Recordings and videos are read again.

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

- `scripts/make-demo.sh`: the demo collection lives outside the repository (docs/ranking.md says how to make it).

## [0.1.3] — 2026-10-14

### Changed

- Explain the DigUp relationship, ranking changes, terminal interface, and front-end integration in the README.
- Preserve Abdur Rahim's MIT notice with the reference test files and in the project license.
- Describe terminal and front-end use in the generated Homebrew cask.

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
