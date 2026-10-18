# Design: indexing and search

Status: agreed on 2026-10-17. Steps 1 to 8 of the build order are built: the config, with depth in place of size and
length limits, the engine, the vector store, search in each of its forms with `auto` choosing, indexing's memory budget,
the protocol, the query path in its stages, and the index behind one facade. It replaces the earlier design with several indexes.

After step 5 the plan changed. The first release is that core, its code structured as docs/architecture.md describes,
and text cut by its form. Projects, text detection on every image, the measured ranking values, metadata and result
units wait until a measurement shows they help: they add parts, and nothing has shown yet that search gets better from
them. This document keeps their design for then.

The goal is the best search there is for everything on a Mac, by meaning across text, pictures, sound and video, while
being careful with memory and fast. It has to run well on an 8 GB MacBook and use what a 24 GB M4 has.

Indexing and search are two processes with different questions:

- **Indexing** is always exact. Its memory budget only decides how fast it goes.
- **Search** trades memory, precision and speed. The user chooses, and `auto` picks from the Mac's memory.

There is one index for everything. Nothing here keeps compatibility with today's config or index files. They're
rebuilt.

## One index

One index gives one ranked list across photos, PDFs, recordings and code, with scores on one scale, no file embedded
twice, and nothing to load or unload by hand. Focus comes from filters, not from separate indexes:

- A search can carry `paths` and `kinds`.
- `search.scopes` names filters you use often, such as `work` for `~/Documents/Work`.
- `sift-it search --here` searches the current folder.

Ranking is tuned on one eval set that covers everything, with overrides per kind. Ranking can't be set per folder,
because two results in one list would no longer be comparable.

## Indexing

The daemon reads files with the full models: text, image and sound, and speech in the `sift-it-transcribe` helper.
Every vector it writes is f32, and what it produces never depends on the Mac it runs on.

`indexing.memory_mb`, `auto` by default, decides throughput only:

- how many workers read files at once, each holding decoded media
- whether the embedding model and the speech helper run side by side or one after the other

At `auto` it's a quarter of the Mac's memory. Side by side takes both models, about 1 GB each, and two readers. On
8 GB (2 GB for indexing) the models take turns. On 24 GB (6 GB) embedding and transcription run together: a thread
hands the speech helper the recordings already read, and the embedding thread embeds and stores what comes back
between files. Reading takes at most a third of the cores, two to four files at once, and a small budget makes that
fewer, down to one. The vectors are the same either way.

Measured on the 8 GB Mac:

- The embedding model's batch size changes nothing: 17.7 passages a second one at a time, and 17.7, 18.1 and 18.0 in
  batches of 2,048, 4,096 and 8,192 tokens, with the same 0.9 to 1.0 GB footprint. So it isn't part of the budget, and
  stays at 2,048 tokens (an image takes 1,120). Whether a bigger GPU gains from bigger batches,
  `SIFT_IT_BATCH_TOKENS=4096 sift-it bench-embed` shows.
- The speech helper peaks at 1.0 GB with a three-minute stretch, which takes it 12 s.
- Two readers index the testbed in 20 s, one in 29 s, both at a 940 MB peak: what a reader decodes is small next to
  the model. The budget reckons with 128 MB a reader.

The rest stays as it is: FSEvents, a full sync now and then, waiting for a charger or out of Low Power Mode, letting the
model go when there's nothing to do, and, when the models take turns, transcription once everything else is read.

## The vector store

Vectors don't live in SQLite. SQLite keeps files, segments, their text, the keyword index and metadata, and for each
segment the partition and row that hold its vector. The vectors live in flat files beside it:

```
vectors/
  main.<generation>.f32      everything but code: 768 floats per row
  main.<generation>.f16      the same rows at half precision
  main.<generation>.sketch   the same rows as 96-byte sketches
  <project>.<generation>.*   the same three files for each project's code
```

- **Partitions.** Everything outside code is one partition, and each project's code is one of its own. The code of an
  inactive project is never mapped, and removing a project removes its files. Until projects exist (later), all code
  is one partition, `code`.
- **Three forms, written once.** For every vector the indexer appends the f32 row, the f16 row and the sketch. Every
  value of `search.vectors` then maps a file that's ready to scan, and switching between them costs nothing. The price
  is 1.6 KB of disk per vector on top of the f32 row's 3 KB.
- **Sketches.** A sketch has one bit per dimension, set when the value is above the partition's mean. SQLite keeps the
  mean the sketches were made against, and the sums of the rows and their squares. When the partition's mean has moved
  so far that about 1% of the bits would come out differently, the partition is due its next generation, as below. A
  sketch file and its mean change together that way, even across a crash. With no dead rows, the next generation's
  f32 and f16 files are links to the current ones, and only the sketches are written.
- **Append, then commit.** The indexer appends a segment's rows before it commits the segment in SQLite, so SQLite never
  points at a row that isn't written. A crash between the two leaves rows nothing points at.
- **Compaction.** Rows of removed segments stay until a quarter of a partition is dead. Then the indexer, when idle,
  writes the partition's next generation, moves its rows in SQLite in one transaction, and deletes the old files. A
  trigger counts a partition's dead rows as their segments go.
- **No copies.** The engine maps the files read-only and scans them in place. Their pages belong to the files, so under
  memory pressure macOS drops them and reads them again later, where memory it had copied into would be swapped out.
  Loading reads only the row table from SQLite and the median sample. The f16 rows are scanned as they are, converted
  to f32 in registers.

## Search

One resident process, the engine, answers every search.

- It runs as its own LaunchAgent, apart from the daemon. A crash in a reader must not take search down, and search
  must not wait for a charger.
- It speaks today's JSON lines over a Unix socket in the data folder. `sift-it serve` becomes a relay between stdio and
  that socket, and starts the engine when it isn't running. `sift-it search` and the TUI go through the engine too.
- The engine reports its build when a client connects: its version and the binary it runs from, so a new build while
  developing counts too. When it differs from the client's, the client stops the engine and starts its own, through
  launchd when the LaunchAgent runs that binary.
- Results carry a path, kind and location. Opening them is up to the front end, and so are previews: `serve` renders
  them, since drawing a video frame or a PDF page is reader code, which stays out of the engine.
- While the model and the vectors load, a search by meaning is answered by words, and an event says when meaning is
  there. A request can say `wait` instead, as `sift-it search` does.

### Modes

A search is by `words` or by `meaning`, set per request with a default in `search.mode`.

- `words` uses only SQLite's full-text index. It needs no encoder and no vectors, so it holds almost nothing in memory.
- `meaning` uses the text encoder and the vectors. Words still count, since names and numbers are found by words.

### What the engine holds

Nothing loads before it's needed. The first search by meaning loads the encoder and maps the main partition. The first
search of code maps the partitions of active projects. What's loaded stays until an `unload` request, a change of
settings, or the engine stopping. There are no idle timers.

`search.vectors` decides which form a search scans:

| Value | Scanned per search, at 300,000 vectors | Scores |
|---|---|---|
| `exact-f32` | 920 MB | exact |
| `exact-f16` | 460 MB | nearly exact |
| `sketch` | 29 MB, plus 9,400 f32 rows (a 32nd) | exact for the candidates |

An exact scan reads every row, so it's only fast while those pages stay in memory. `auto`, the default, picks the most
precise form whose scan fits `search.memory_mb`. That budget is 10% of the Mac's memory unless set: about 800 MB on
8 GB, enough for exact f32 up to some 260,000 vectors and exact f16 up to some 520,000. The demo collection, 6,500
vectors, is always scanned exactly.

A sketch search goes in two steps. First it estimates every row's cosine from its sketch, with the kind and path filters
as a mask. The query stays exact: each dimension counts by how much of the query is in it and how far the partition's
rows spread in it (from the sums SQLite keeps), rounded to 16 steps, so a row's estimate is five counts of bits. That
takes a few milliseconds. Then it scores the closest exactly from their f32 rows, and the segments the query's words are
in, and ranks them the way search does today. It scores 4,096 candidates, or a 32nd of the rows in a larger index: as
many rows as the sketches are bytes. Searches typed letter by letter share most of their candidates, so after the first
letter those rows are already in memory. The query's median, which the lift needs, comes from a fixed sample of 8,192
rows that the engine copies into memory at load, 24 MB.

llama.cpp maps the model's weights from their file the same way. Whether Metal uses them without a copy, so macOS can
drop them under pressure too, `footprint` will show.

`sift-it bench-search` measures all three forms: memory, time per search cold and warm, and how much of the exact top
20 the sketch search keeps. `sift-it eval --compare` measures that last part on real queries, with 256 to 8,192
candidates.

Measured so far, on the 8 GB Mac:

- 200,000 vectors (`bench-search`): 13 ms per search exact at f32, 11 ms at f16, 7 ms with sketches and 6,250
  candidates. The engine's footprint is about 60 MB in each, the sample included; mapped rows aren't in it.
- Rust's books (2,100 pages, 7,400 vectors, all on one subject, so many rows come close to a query) with 45 queries:
  sketches keep 99.3% of the exact top 20 with 256 candidates, 99.9% with 512, and all of it from 1,024. By segment
  it takes more: the segments the exact top 20 found by meaning came within the first 1.9% of the rows by sketch for
  half the queries, 3.9% for 90%, and 11.9% for all. A file is found by any of its segments that's close, so a missed
  segment rarely costs it its place.
- Centering on the partition's mean, with the spread weighing each dimension, is enough for EmbeddingGemma 2's
  vectors: the numbers above come from that alone.

## Projects

A folder with its own git repository is a project. Its code is handled apart from everything else.

- By default a project is indexed as a summary, put together from what it says about itself: its README and docs, the
  description in `package.json` or `Cargo.toml`, its folder structure, and its recent commit messages. A handful of
  passages, enough to find the project in any search. A summary written by a language model would need a model sift-it
  doesn't have.
- An active project gets its code indexed in full. A project is active when it was edited in the last
  `projects.active_days`, 14 by default, or when it's pinned with `sift-it projects pin <path>`.
- When a project goes quiet, its code stays in the index but leaves the engine's memory. Coming back makes it
  searchable at once, with nothing embedded again.
- A search of code, by `code:`, the code filter, or `--here` inside a project, covers the code of active projects only.
  Other searches never show code, only project summaries.

Pictures, PDFs and recordings inside a project are indexed like any other file.

## Configuration

Each Mac has one `config.json`. The data folder holds it with `index.sqlite`, `status.json`, `indexer.log`, `models/`,
`previews/` and the engine's socket.

Some settings express intent: what gets indexed and how deep, timing, memory. The user decides those. Others set quality:
how files are read and how results rank. An eval decides those. A quality constant becomes a setting only once an eval
shows a reason to vary it.

### Sections and what a change costs

| Section | Holds | After a change |
|---|---|---|
| `sources` | paths, ignore patterns, which file types belong to which kind | a sync |
| `reading` | depth and reading values per kind, with overrides | the affected files are read again |
| `metadata` | how much each metadata field counts | the next search |
| `ranking` | the search constants, the result unit, overrides per kind | the next search |
| `projects` | when a project counts as active | the next sync |
| `indexing` | memory, battery, timing | at once |
| `search` | mode, vectors, memory, scopes | the next search, or reloading the vectors |

The index stores, per file, a fingerprint of how it was read: its reader's version and the reading values that reader
uses, as the cascade resolves them for that file. A sync compares it like the size and the modified time, so a change
queues exactly the files it affects, wherever in the cascade it was made. Whether speech is written down stays out of
it: that comes after reading, so moving between `meaning` and `speech` adds or drops transcripts without reading a
file again. `embedder::vector_source` and `speech::source` still cover the models.

### The cascade

A value in `reading` resolves in four steps, and the last one that sets it wins: the global value, the kind, the file
type, then the path rules in order. Depth can be set for everything, for one kind, for any file type, or for one
folder.

### Depth

`ignore` decides what's in the index. `reading.depth` decides how deep a file in it is read, in three stages that
build on each other:

| Depth | A file is found by | Costs |
|---|---|---|
| `words` | its name, its metadata and the words in it: text, a PDF's text layer, text on pictures | little, and no model |
| `meaning` | also what it means: passages, pictures, video frames and sound, embedded | most of the work |
| `speech` | also what's said in it, written down and embedded | time per hour of sound |

`speech`, the default, is everything. For kinds without sound it's the same as `meaning`. A folder of raw footage can
be `words`, a folder of music `meaning`.

There are no size or length limits. A four-hour podcast has to be found as well as a short memo, so every file is
read in full at its depth. What limits used to guard is handled where it arises:

- **Waiting.** The queue goes from the cheapest file to the dearest, by an estimate of each file's work made when it's
  queued. Most files are searchable soonest, and the podcast comes last, but whole. A file that just changed goes
  first.
- **Memory.** Readers stream: text in pieces, PDFs by page, sound and video by window. A big file's segments are
  written in batches as they come, the way transcripts already are, so memory doesn't grow with the file. A search can
  meanwhile find the part that's in.
- **A signal.** `sift-it status` and the front ends show the work still queued, and the heaviest files with their
  estimate. The user can act on it, with `ignore` or a lower depth, or let it run.

### Kinds

Kinds are image, audio, video, plaintext, document, structured and code. `sources` maps file types to kinds, with
built-in defaults. A PDF is a document. Screenshot isn't a kind but a metadata field.

| Kind | Settings |
|---|---|
| image | `tokens`, `text_tokens` |
| audio | none beyond depth |
| video | shot change, keep every n seconds, sound on or off (no sound, no speech). Frames take the image settings |
| document | images inside the document. Scanned pages get OCR |
| plaintext | passage size |
| code | passage size, cut at definitions |
| structured | passage size, ignore patterns for keys and columns |

Vision's text detection runs on every image, video frames included. An image with text gets `text_tokens` and full OCR.
One without gets `tokens` and no OCR. A photo of a whiteboard gets the higher budget, a screenshot of a holiday photo
the lower one. OCR has no on/off setting, and skipping recognition on images without text keeps it affordable for a
photo library.

Structured files are csv, tsv, json, jsonl, yaml and toml. They're cut like markdown into whole records, rows or
top-level keys that fit the passage size. Ignore patterns for keys and columns, like `**.id` or `created_at`, keep
identifiers and timestamps out of the passages.

Code is cut by its structure, not at a fixed length: a top-level definition (a function, a class, an impl block) is
a passage, with the comments and attributes right above it. Indentation finds them, so it works for any language
without a parser: a line at the outer level starts a definition, and a closing bracket there ends one. Small
definitions next to each other are joined up to the passage size. One larger than the maximum is cut at its inner
definitions, then at blank lines. A notebook cell is cut the same way.

### Example

```json
{
  "sources": {
    "paths": ["~/Documents", "~/Desktop", "~/Movies", "~/developer"],
    "ignore": ["**/Exports/**", "*.psd"],
    "kinds": {"image": ["jpg", "jpeg", "heic", "png"], "video": ["mov", "mp4"]}
  },
  "reading": {
    "depth": "speech",
    "image": {"tokens": 140, "text_tokens": 280},
    "video": {"shot_change": 14, "keep_every_s": 30, "sound": true},
    "types": {"gif": {"depth": "words"}},
    "rules": [{"path": "~/Movies/Lectures/**", "keep_every_s": 10}, {"path": "~/Movies/Raw/**", "depth": "words"}]
  },
  "metadata": {"path": 1.0, "taken": 1.0, "modified": 0.5, "tags": 1.0, "camera": 0.5},
  "ranking": {"unit": "file", "metadata_bonus": 0.02, "shown_lift": 0.09, "strong_lift": 0.13},
  "projects": {"active_days": 14},
  "indexing": {"memory_mb": "auto", "on_battery": true, "quiet_ms": 1500, "full_sync_min": 30, "release_model_s": 60},
  "search": {"mode": "meaning", "vectors": "auto", "memory_mb": "auto", "scopes": {"work": {"paths": ["~/Documents/Work"]}}},
  "log": {"max_mb": 4}
}
```

The `sources.kinds` and `ranking` sections are trimmed. `ranking` holds the values the ranking uses (`src/query/rank.rs`),
except the two that sift-it measures.

These stay in the code: the models' facts (768 dimensions, 16 kHz sound, 30-second windows, patch size), SQLite busy
timeouts, the power-check interval, and decode sizes, which follow from the token budgets.

## Results

`ranking.unit` sets what a result is, and a request can override it.

- `file`, the default, is today's behavior. A file scores by its best segment, less a penalty for its number of
  segments.
- `chapter` groups segments into chapters. The penalty counts the chapter's segments, and the hit spans the chapter.
- `chunk` ranks every segment on its own, with no length penalty, because no result is the best of many. The cap per
  file stays, so one book can't fill the list.

Chapters come from explicit structure when a file has it: PDF outlines, markdown headings, chapter markers in audio and
video. Otherwise a chapter ends where the similarity between neighbouring segments drops sharply. One threshold serves
every kind. Text nearly always has structure from its formatting, so these breaks matter most for recordings without
chapter markers, like a badly tagged podcast. The engine computes them from the stored vectors, so tuning the
threshold never embeds anything again.

## Metadata

| Kind | Fields |
|---|---|
| all files | path, created, modified, date added, Finder tags, downloaded from, is screenshot |
| photos | taken, GPS position, camera and lens, size in pixels |
| video | recorded, duration, GPS position, resolution |
| audio | duration, title, artist, album, genre, year |
| documents | title, author, subject, keywords, pages |

Readers extract every cheap field while they have the file open anyway. Finder tags and the download source are
extended attributes. None of it depends on Spotlight. The fields go into a `metadata` table, one typed value per file
and field, with a keyword index of their own.

Metadata counts toward ranking at a low weight. A query word that matches a field adds a small bonus, so the "2017" in
"tax return 2017" can match a folder in the path or the year a file was modified. Dates match by year and month.
`ranking.metadata_bonus` sets the base weight and `metadata` scales it per field, with 0 for a field that shouldn't
count. Operators such as `taken:2017`, `author:dickens` and `tag:work` narrow a search exactly, and are always
optional.

## Tuning

`sift-it eval <queries.json>` measures search against known answers. Tuning happens outside sift-it: a person or an
agent edits `config.json` and runs the eval again. Keep part of the queries out of tuning and judge on those, or the
gain looks bigger than it is.

Two values describe the collection rather than a preference, so sift-it measures them:

- the prior median, the typical median of a query's scores, today 0.55 from the testbed
- the length penalty, which follows from the typical spread of a query's scores. `docs/ranking.md` derives today's
  0.0125 from a spread of 0.03.

The engine measures both when it loads the vectors, with a fixed set of probe queries. A collection of only a few
segments keeps the built-in values. `sift-it status` shows what was measured.

Today the median counts code vectors even in searches that never show code. Once code is only in memory for code
searches, it doesn't, so ranking shifts even with today's values. Run `make eval` and the demo queries after that
change.

## Not in scope

- Opening files in apps. `sift-it open` and the TUI keep today's behavior, with no settings.
- Cloud-only files and the Photos library.
- OCR other than Vision. A `provider` setting comes when there's a second one.
- Later: images inside documents, place names from GPS positions, project summaries written by a language model.

## Build order

Built:

1. Config: one `config.json` with these sections and the cascade, and depth in place of limits. Constants move in
   with today's values, and `make eval` must still give 44/44.
2. The engine: socket, `serve` as relay, version check, the two modes, loading on first need, `unload`. CLI and TUI
   move onto it.
3. The vector store: partitions, the three forms, append then commit, compaction. `search.vectors` with the two exact
   forms, and `bench-search` measuring them.
4. The sketch search, the median sample, and `auto`. Measure the top-20 overlap on the demo collection.
5. `indexing.memory_mb`, with embedding and transcription side by side when it allows.
6. The protocol: its types in a crate of their own (`crates/protocol`), documented (`docs/protocol.md`). The engine
   reports the indexer's progress and takes settings and folders through it.
7. The query path: understand, retrieve, rank and present as modules (`src/query/`), ranking pure.
8. The index: one facade with a writer and readers over the catalog and the vectors (`src/index/`).

Next, for the first release. Steps 9 to 12 change no behavior (docs/architecture.md, "The way there"):

9. Extracting apart from segmenting.
10. The ingest path: stages with owners, a scheduler, and speech as an extractor on a lane of its own.
11. Resources: the plans for search and ingest in one module.
12. Crates where they pay: llama.cpp's binding, and the modules as docs/architecture.md lays them out.
13. Text cut by its form: prose and Markdown at paragraphs and headings, code at its definitions, structured files
    (csv, tsv, json, jsonl, yaml, toml) at whole rows, records or top-level keys. Eval questions for code and
    structured files measure it.

Later, when a measurement shows it helps:

- Projects: detection, summaries, active and pinned, code loaded only for code searches.
- Text detection on every image, deciding the token budget and OCR. It costs a detection on every picture.
- The measured prior median and length penalty.
- Metadata: the table, its weight in ranking, operators.
- Result units and chapters.

## Open questions

- How the candidates a sketch search needs grow with an index past 10,000 vectors. Its best matches stand further above
  the rest as it grows, so the share it takes should fall, and a 32nd would keep up; a large real collection will
  tell. `sift-it eval --compare docs/demo-queries.json` measures the demo collection.
- What a project summary needs: which files, and how many commit messages.
- The chapter break threshold, tuned on recordings without chapter markers.
- How a word found only in metadata counts when the query has a number, which today makes the words decide. "beach
  2017" belongs in the eval set.
- Whether 10% of memory is the right default search budget.
- Whether side by side is faster overall on 24 GB, where both models share the GPU: the time to index a folder of
  recordings with `indexing.memory_mb` at 2048 and at 6144 tells.
