# Design: indexing and search

What sift-it does, and why it's built the way it is. `docs/architecture.md` says where each part lives in the code.
What isn't built yet, and waits until a measurement shows it helps, is under [Later](#later): it adds parts, and
nothing has shown yet that search gets better from it.

The goal is the best search there is for everything on a Mac, by meaning across text, pictures, sound and video, while
being careful with memory and fast: it runs well on a Mac with little memory and uses what a bigger one has.

Indexing and search are two processes with different questions:

- **Indexing** is always exact. Its memory budget only decides how fast it goes.
- **Search** trades memory, precision and speed. The user chooses, and `auto` picks from the Mac's memory.

There is one index for everything. An index of another version isn't migrated: it's dropped and built again.

## One index

One index gives one ranked list across photos, PDFs, recordings and code, with scores on one scale, no file embedded
twice, and nothing to load or unload by hand. Focus comes from filters, not from separate indexes: a search can carry
the kinds it wants.

Ranking is tuned on one eval set that covers everything. Ranking can't be set per folder, because two results in one
list would no longer be comparable.

## The models

Three models, 1.5 GB together: EmbeddingGemma 2's text model (310 MB) and its vision and audio encoders (555 MB, one
file), both GGUF files, and Parakeet Ultra for speech (632 MB): FluidInference's conversion for Core ML, with an int8
encoder, which is four compiled Core ML models and a vocabulary.

- **Stored once.** They're kept in the Hugging Face cache that other apps share (`$HF_HUB_CACHE`, else
  `$HF_HOME/hub`, else `~/.cache/huggingface/hub`), in its layout. The hub names a file's blob after its content: the
  SHA-256 of a file it keeps in LFS, else its git blob id. So a model of one file is there when any repository's folder
  in the cache holds a whole blob of that name, whichever app or revision put it there. The speech model is a folder:
  it's there when its revision's folder in its repository holds each of its files whole, since that folder is what
  Core ML loads (compiled models load through the cache's links as they are). Handy's GGUF of Parakeet Ultra served
  sift-it while it ran transcribe.cpp, and doesn't any more. What's missing is downloaded at a pinned revision and
  checked against its hash before it gets that name. sift-it only adds what's missing to the cache and never removes
  anything, and uninstalling it leaves the files there for the other apps.
- **Loaded once.** llama.cpp maps the weights from their files and Metal uses them in place, so the engine and the
  indexer share the text model's pages, and macOS can drop them when memory runs short and read them again. Only the
  vision and audio encoders are read into memory of their own: llama.cpp's mtmd copies them. Core ML gives the Neural
  Engine a copy of the speech model (607 MB), which macOS counts to the speech helper but not in its footprint.
- **Only what a step needs.** A search by meaning loads the text model, with a context made for one query. The
  indexer loads the text model for text, the vision encoder while it reads pictures and video frames, the audio
  encoder while it reads sound, and the speech helper while it writes down what's said. Both encoders are in one file,
  and each loads on its own (a switch sift-it adds to mtmd: `scripts/llama.cpp.patch`). When the reading is over, the
  encoders go, so on a small budget the speech helper runs beside the text model alone.

Measured on an M5 with 24 GB, on part of the demo folder, as footprint: the memory a process has of its own. The
mapped model files come on top, as file pages in memory, counted once however many processes map them.

- The engine holds 162 MB after a search by meaning, and 49 MB once it lets the model go. Its context for one query
  of up to 256 tokens takes 32 MB of compute buffers, where a context for documents (16 passages, or 2,048 tokens)
  takes 276 MB. Most of the rest is llama.cpp's tokenizer: 800,000 small allocations for its 262,000 tokens.
- The indexer holds 338 MB while it reads text, 544 MB while it reads photos, 601 MB while it reads music, and 877 MB
  while it reads videos, which take both encoders. The vision encoder holds 289 MB and loads in 51 ms; the audio
  encoder holds 321 MB and loads in 69 ms (`sift-it bench-embed`, the file in memory).
- While speech is written down with the models taking turns, the indexer holds 360 MB and the speech helper 110 MB,
  and the Neural Engine 607 MB for it.
- The engine and the indexer reading text at once hold 500 MB together.
- ggml's Metal backend keeps its buffers resident for three minutes after each call (residency sets, macOS 15 and
  later), renewed by a thread that wakes every 5 ms for as long as the process lives, idle or not: 174 times a second in
  an idle engine. In the engine, one search by meaning kept about 330 MB of the GPU's memory wired for three minutes.
  sift-it turns them off (`GGML_METAL_NO_RESIDENCY`): Metal makes memory resident for the calls that use it, nothing
  stays wired between calls, and the idle engine doesn't wake. Embedding is as fast, within the noise of three rounds of
  `sift-it bench-embed` (medians, without and with): a query 10.7 and 11.9 ms, 44 and 45 passages a second, 30 s of
  sound 309 and 318 ms. A shorter keep-alive (`GGML_METAL_RESIDENCY_KEEP_ALIVE_S`) would let the memory go sooner, but
  the thread would keep waking.
- The environment llama.cpp wants is set first thing, while one thread runs (`embedder::set_environment`).
  - **What's set:** the residency switch above, and AGX_RELAX_CDM_CTXSTORE_TIMEOUT=1, which ggml's Metal backend adds
    itself when it registers.
  - **Why it matters:** macOS reallocates the environment's list to add a variable, so a process another thread starts
    at that moment can get a list that was just freed. When the indexer loaded the model, its readers were starting
    textutil, and two or three documents failed with "Bad address" now and then.
  - **Now:** set beforehand, the backend's setenv rewrites the same value in place. `make selftest` fails if loading
    the models changes the environment.

## Indexing

The indexer reads files with the full models: text, image and sound, and speech in the `sift-it-transcribe` helper.
Every vector it writes is f32, and what it produces never depends on the Mac it runs on.

`indexing.memory_mb`, `auto` by default, decides throughput only:

- how many workers read files at once, each holding decoded media
- whether the embedding model and the speech helper run side by side or one after the other

At `auto` it's a quarter of the Mac's memory. Side by side takes both models, about 1.2 and 0.75 GB (the text model's
mapped file, and the Neural Engine's copy of the speech model), and two readers. When the budget holds that, embedding
and transcription run together: a thread hands the speech helper the recordings already read, and the embedding thread
embeds and stores what comes back between files. With less, the models take turns, and what's said is written down once
everything else is read, beside the text model alone. Reading takes at most a third of the cores, two to four files at
once, and a small budget makes that fewer, down to one. The vectors are the same either way.

Measured:

- The embedding model's batch size changes nothing: 17.7 passages a second one at a time, and 17.7, 18.1 and 18.0 in
  batches of 2,048, 4,096 and 8,192 tokens, with the same footprint. So it isn't part of the budget, and stays at
  2,048 tokens (an image takes 1,120). Whether a bigger GPU gains from bigger batches,
  `SIFT_IT_BATCH_TOKENS=4096 sift-it bench-embed` shows.
- The speech helper holds 100 to 120 MB of its own while it writes down a three-minute stretch, which takes it under a
  second ([Speech](#speech)).
- Side by side is the faster: three podcasts and four videos (124 minutes of sound) are indexed in 138 s, against 154 s
  with the models taking turns. The reading beside speech takes 128 s, against 120 s alone, since speech has the
  Neural Engine and only its passages wait for the GPU. Beside transcribe.cpp, which shared the GPU, the reading took
  158 s, and the whole 224 s, against 240 s taking turns.
- Two readers index the testbed in 20 s, one in 29 s, at the same peak: what a reader decodes is small next to the
  model. The budget reckons with 128 MB a reader.

The indexer follows the folders with FSEvents and makes a full sync now and then. It waits for a charger (when
`indexing.on_battery` is off) and out of Low Power Mode, and lets the model go when there's nothing to do.

### Speech

What's said is written down by Parakeet Ultra on the Neural Engine, through FluidAudio (Core ML), in the speech
helper. FluidAudio decodes a chunk of about three minutes in 15-second windows, each starting with the end of the one
before (an 80 ms mel prefix, its `melChunkContext`), rather than at a silence nearby. The helper hands over each word
with its time, and a passage ends with the first sentence that brings it to 160 characters, or where a pause of more
than 4 s comes: 92% of the passages end with a sentence, where half of transcribe.cpp's 28-second windows did.

Measured on an M5 with 24 GB, against transcribe.cpp 0.4.1 on the GPU with the same model:

- **Speed:** in the indexer, its passages embedded, about 220 times as fast as the sound plays: three podcasts and
  four videos (124 minutes) in 34 s, against 118 s (63 times). On its own, 235 to 286 times, against 83 to 93.
- **Energy:** 116 J an hour of sound, on battery, against 553 J.
- **Memory:** 100 to 120 MB of its own, plus the Neural Engine's copy of the model, 607 MB, and the 32 MB it maps of
  what Core ML compiled for the Neural Engine. That copy doesn't show in its footprint: `footprint` lists it apart, as
  "owned physical footprint (neural)", and proc_pid_rusage as `ri_neural_footprint`. The first time a Mac loads the
  model, the Neural Engine compiles it (about 8 s), and Core ML keeps that in `~/Library/Caches/sift-it-transcribe`.
- **Accuracy:** on FLEURS Dutch (read sentences, with human transcripts) 5.65 to 5.67% WER, against 5.49% (5.46%
  published). On Dutch conversation (two NOS "De Dag" episodes), about 0.2 points of WER behind: 12 clear errors in 60
  judged spans, against 10, and 19 without the mel prefix. On the demo's English podcasts and videos, the two differ
  in 5.8% of the words: fillers, small words, and how a number is written.
- **Numbers** come out as the model learned to write them, in either engine: most as digits (years, prices, times,
  "1,500", "3,5 procent", "12 maart 2024"), small counts and ordinals often as words ("two tickets", "tien jaar",
  "derde"). Where one engine writes "15" and the other "fifteen", the model's choice tipped the other way, in Dutch
  as in English. FluidAudio's inverse text normalization (NeMo's) isn't used: it has no Dutch, and in English it also
  writes "one of the" as "1 of the", and "at six and eight thirty" as "at 44".
- **The Neural Engine is shared** with Vision's OCR, which the reading uses for screenshots and scans (171 MB of the
  Neural Engine's memory, in the indexer). The two run at once without failing: 1,086 screenshots read beside speech
  (362 in the indexer, at `auto`, and 724 beside a helper writing down the demo's 30 podcasts without a pause), none
  failed, and the helper wrote down 469 chunks without an error. They do slow each other, OCR more than speech:
  0.53 s a screenshot alone, 1.17 s beside speech, while speech keeps about 200 times its sound's speed.

## The vector store

Vectors don't live in SQLite. SQLite keeps files, segments, their text, the keyword index, and for each segment the
partition and row that hold its vector. The vectors live in flat files beside it:

```
vectors/
  main.<generation>.f32      everything but code: 768 floats per row
  main.<generation>.f16      the same rows at half precision
  main.<generation>.sketch   the same rows as 96-byte sketches
  code.<generation>.*        the same three files for code
```

- **Partitions.** Everything outside code is one partition, and code is one of its own.
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

- It runs as its own LaunchAgent, apart from the indexer. A crash in a reader must not take search down, and search
  must not wait for a charger.
- It speaks JSON lines (`docs/protocol.md`) over a Unix socket in the data folder. `sift-it serve` is a relay between
  stdio and that socket, and starts the engine when it isn't running. `sift-it search` and the TUI go through the
  engine too.
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

Nothing loads before it's needed. The first search by meaning loads the text model and maps the vectors. What's loaded
stays until an `unload` request or the engine stopping. A change of settings is taken in at the next search without
loading anything again, since every form of the vectors is mapped. There are no idle timers.

`search.vectors` decides which form a search scans:

| Value | Scanned per search, at 300,000 vectors | Scores |
|---|---|---|
| `exact-f32` | 920 MB | exact |
| `exact-f16` | 460 MB | nearly exact |
| `sketch` | 29 MB, plus 9,400 f32 rows (a 32nd) | exact for the candidates |

An exact scan reads every row, so it's only fast while those pages stay in memory. `auto`, the default, picks the most
precise form whose scan fits `search.memory_mb`. That budget is 10% of the Mac's memory unless set; 800 MB holds exact
f32 for some 260,000 vectors and exact f16 for some 520,000.

A sketch search goes in two steps. First it estimates every row's cosine from its sketch, with the kind filter as a
mask. The query stays exact: each dimension counts by how much of the query is in it and how far the partition's rows
spread in it (from the sums SQLite keeps), rounded to 16 steps, so a row's estimate is five counts of bits. That takes a
few milliseconds. Then it scores the closest exactly from their f32 rows, and the segments the query's words are in,
and ranks them as an exact search does. It scores 4,096 candidates, or a 32nd of the rows in a larger index: as many
rows as the sketches are bytes. Searches typed letter by letter share most of their candidates, so after the first
letter those rows are already in memory. The query's median, which the lift needs, comes from a fixed sample of 8,192
rows that the engine copies into memory at load, 24 MB.

llama.cpp maps the model's weights from their file the same way, and Metal uses them in place: they're file pages in
memory, not in the engine's footprint, and macOS can drop them under pressure too ([The models](#the-models)).

`sift-it bench-search` measures all three forms: memory, time per search cold and warm, and how much of the exact top
20 the sketch search keeps. `sift-it eval --compare` measures that last part on real queries, with 256 to 8,192
candidates.

Measured:

- 200,000 vectors (`bench-search`): 13 ms per search exact at f32, 11 ms at f16, 7 ms with sketches and 6,250
  candidates. The engine's footprint is about 60 MB in each, the sample included; mapped rows aren't in it.
- Rust's books (2,100 pages, 7,400 vectors, all on one subject, so many rows come close to a query) with 45 queries:
  sketches keep 99.3% of the exact top 20 with 256 candidates, 99.9% with 512, and all of it from 1,024. By segment
  it takes more: the segments the exact top 20 found by meaning came within the first 1.9% of the rows by sketch for
  half the queries, 3.9% for 90%, and 11.9% for all. A file is found by any of its segments that's close, so a missed
  segment rarely costs it its place.
- Centering on the partition's mean, with the spread weighing each dimension, is enough for EmbeddingGemma 2's
  vectors: the numbers above come from that alone.

## Configuration

Each Mac has one `config.json`. The data folder holds it with `index.sqlite`, `vectors/`, `status.json`,
`indexer.log`, `previews/` and the engine's socket. The model files are in the Hugging Face cache (see
[The models](#the-models)).

Some settings express intent: what gets indexed and how deep, timing, memory. The user decides those. Others set
quality: how files are read and how results rank. An eval decides those. A quality constant becomes a setting only once an eval
shows a reason to vary it.

### Sections and what a change costs

| Section | Holds | After a change |
|---|---|---|
| `sources` | paths, ignore patterns, which file types belong to which kind, whether code is read | a sync |
| `reading` | depth and reading values per kind, with overrides | the affected files are read again |
| `ranking` | the search constants | the next search |
| `indexing` | memory, battery, timing | at once |
| `search` | mode, vectors, memory | the next search, or reloading the vectors |
| `log` | the size of the indexer's log | at once |

The index stores, per file, a fingerprint of how it was read: its reader's version and the reading values that reader
uses, as the cascade resolves them for that file. A sync compares it like the size and the modified time, so a change
queues exactly the files it affects, wherever in the cascade it was made. Whether speech is written down stays out of
it: that comes after reading, so moving between `meaning` and `speech` adds or drops transcripts without reading a
file again. Two more records cover the models, and each brings back only what it covers. `embedder::VECTOR_SOURCE`
covers every vector: another embedding model reads every file again, and embeds what's written down of recordings as
it was written down, without writing it down again. `speech::SOURCE` covers what's written down: another speech
model writes down every recording again, and nothing else.

### The cascade

A value in `reading` resolves in four steps, and the last one that sets it wins: the global value, the kind, the file
type, then the path rules in order. Depth can be set for everything, for one kind, for any file type, or for one
folder.

### Depth

`ignore` decides what's in the index. `reading.depth` decides how deep a file in it is read, in three stages that
build on each other:

| Depth | A file is found by | Costs |
|---|---|---|
| `words` | its name, its tags and the words in it: text, a PDF's text layer, text on screenshots and scans | little, and no model |
| `meaning` | also what it means: passages, pictures, video frames and sound, embedded | most of the work |
| `speech` | also what's said in it, written down and embedded | time per hour of sound |

`speech`, the default, is everything. For kinds without sound it's the same as `meaning`. A folder of raw footage can
be `words`, a folder of music `meaning`.

There are no size or length limits. A four-hour podcast has to be found as well as a short memo, so every file is
read in full at its depth. What a limit would guard is handled where it arises:

- **Waiting.** The queue goes from the cheapest file to the dearest, by an estimate of each file's work made when it's
  queued. Most files are searchable soonest, and the podcast comes last, but whole. A file that just changed goes
  first.
- **Memory.** Readers stream: text in pieces, PDFs by page, sound and video by window. A big file's segments are
  written in batches as they come, like transcripts, so memory doesn't grow with the file. A search can meanwhile find
  the part that's in.
- **A signal.** `sift-it status` and the front ends show the work still queued, and the heaviest files with their
  estimate. The user can act on it, with `ignore` or a lower depth, or let it run.

### Kinds

Kinds are plaintext, document, structured, image, screenshot, audio, video and code. `sources` maps file types to
kinds, with built-in defaults. A PDF is a document. A screenshot is a picture macOS (or a screenshot tool) marked as
one: it's read for its text as well as what it shows, and takes the image settings.

| Kind | Settings |
|---|---|
| image | `tokens`, and `text_tokens` for screenshots |
| audio | none beyond depth |
| video | shot change, keep every n seconds, sound on or off (no sound, no speech). Frames take `tokens` |
| document | `text_tokens` for scanned pages, which get OCR |
| plaintext | passage size, cut at headings |
| code | passage size, cut at definitions |
| structured | passage size, cut at records |

Text is cut where its form breaks (`src/ingest/segment/`). A part between two breaks that fits the maximum stays
whole, and a longer one is cut at its next weaker breaks. Parts next to each other are joined up to the passage size,
but the pieces of a part cut up aren't joined with what's around it, and they're embedded with the part's first line
(its heading, its signature, its key) before them, so the model sees what they belong to.

Prose is cut at paragraphs, and Markdown and plain text also at their headings (`#`, or underlined): a section that
fits the maximum is one passage, and a heading stays with what follows it. Text before the first heading, and text
with none, is paragraphs joined up to the passage size. A paragraph longer than the maximum is cut into pieces of
about the passage size at line ends, sentences or spaces.

Code is cut by its structure, not at a fixed length: a top-level definition (a function, a class, an impl block) is
a passage, with the comments and attributes right above it. Indentation finds them, so it works for any language
without a parser: a line at the outer level starts a definition, and a closing bracket there ends one. Small
definitions next to each other are joined up to the passage size. One larger than the maximum is cut at its inner
definitions, then at blank lines. A notebook cell is cut the same way, and a Markdown cell as Markdown.

Structured files are csv, tsv, json, jsonl, yaml and toml. CSV and TSV are cut at their rows, each passage embedded
with the header row; JSON at the records of its outer list or the keys of its outer object, whatever its line breaks;
JSON Lines at its lines; YAML at its top-level keys and items; TOML at its tables. A record too long for the maximum is
cut at its own keys and items. Records are read one by one: their passage size is 120 characters, so only short ones
(a setting's line) go together. A passage of several records blurs what each says: on the testbed, a journal entry
asked for by meaning comes first this way, and second in passages of 600. Lock files (`package-lock.json`,
`pnpm-lock.yaml`) are never read, and in a code project structured files are read only with the code.

### Example

```json
{
  "sources": {
    "paths": ["~/Documents", "~/Desktop", "~/Movies", "~/developer"],
    "ignore": ["**/Exports/**", "*.psd"],
    "kinds": {"image": ["jpg", "jpeg", "heic", "png"], "video": ["mov", "mp4"]},
    "code": true
  },
  "reading": {
    "depth": "speech",
    "image": {"tokens": 140, "text_tokens": 280},
    "video": {"shot_change": 14, "keep_every_s": 30, "sound": true},
    "types": {"gif": {"depth": "words"}},
    "rules": [{"path": "~/Movies/Lectures/**", "keep_every_s": 10}, {"path": "~/Movies/Raw/**", "depth": "words"}]
  },
  "ranking": {"shown_lift": 0.09, "strong_lift": 0.13},
  "indexing": {"memory_mb": "auto", "on_battery": true, "quiet_ms": 1500, "full_sync_min": 30, "release_model_s": 60},
  "search": {"mode": "meaning", "vectors": "auto", "memory_mb": "auto"},
  "log": {"max_mb": 4}
}
```

The `sources.kinds` and `ranking` sections are trimmed. `ranking` holds the values the ranking uses
(`src/query/rank.rs`).

These stay in the code: the models' facts (768 dimensions, 16 kHz sound, 30-second windows, patch size), SQLite busy
timeouts, the power-check interval, and decode sizes, which follow from the token budgets.

## Tuning

`sift-it eval <queries.json>` measures search against known answers. Tuning happens outside sift-it: a person or an
agent edits `config.json` and runs the eval again. Keep part of the queries out of tuning and judge on those, or the
gain looks bigger than it is.

## Not in scope

- Opening files in apps beyond what `sift-it open` and the TUI do, with no settings.
- Cloud-only files and the Photos library.
- OCR other than Vision. A `provider` setting comes when there's a second one.

## Later

Designed, and built when a measurement shows it helps.

### Projects

A folder with its own git repository is a project. Its code is handled apart from everything else.

- By default a project is indexed as a summary, put together from what it says about itself: its README and docs, the
  description in `package.json` or `Cargo.toml`, its folder structure, and its recent commit messages. A handful of
  passages, enough to find the project in any search. A summary written by a language model would need a model sift-it
  doesn't have.
- An active project gets its code indexed in full. A project is active when it was edited in the last
  `projects.active_days`, 14 by default, or when it's pinned with `sift-it projects pin <path>`.
- Each project's code is a vector partition of its own. When a project goes quiet, its code stays in the index but
  leaves the engine's memory. Coming back makes it searchable at once, with nothing embedded again. Removing a project
  removes its files.
- A search of code, by `code:`, the code filter, or `--here` inside a project, covers the code of active projects only.
  Other searches never show code, only project summaries.

Pictures, PDFs and recordings inside a project are indexed like any other file.

Today the median a lift is measured from counts code vectors, even in searches that never show code. Once code is only
in memory for code searches it doesn't, so ranking shifts even with the same values: run `make eval` and the demo
queries after that change.

### Scopes

A search can carry `paths` as well as kinds. `search.scopes` names filters used often, such as `work` for
`~/Documents/Work`, and `sift-it search --here` searches the current folder.

### Text detection on every image

Vision's text detection runs on every image, video frames included. An image with text gets `text_tokens` and full OCR.
One without gets `tokens` and no OCR. A photo of a whiteboard gets the higher budget, a screenshot of a holiday photo
the lower one. OCR has no on/off setting, and skipping recognition on images without text keeps it affordable for a
photo library. It costs a detection on every picture. Screenshot then becomes a metadata field rather than a kind.

### Results

`ranking.unit` sets what a result is, and a request can override it.

- `file`, the default, is how results work now. A file scores by its best segment, less a penalty for its number of
  segments.
- `chapter` groups segments into chapters. The penalty counts the chapter's segments, and the hit spans the chapter.
- `chunk` ranks every segment on its own, with no length penalty, because no result is the best of many. The cap per
  file stays, so one book can't fill the list.

Chapters come from explicit structure when a file has it: PDF outlines, markdown headings, chapter markers in audio and
video. Otherwise a chapter ends where the similarity between neighbouring segments drops sharply. One threshold serves
every kind. Text nearly always has structure from its formatting, so these breaks matter most for recordings without
chapter markers, like a badly tagged podcast. The engine computes them from the stored vectors, so tuning the
threshold never embeds anything again.

### Metadata

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
`ranking.metadata_bonus` sets the base weight and a `metadata` section scales it per field, with 0 for a field that
shouldn't count. Operators such as `taken:2017`, `author:dickens` and `tag:work` narrow a search exactly, and are
always optional.

### Measured ranking values

Two values describe the collection rather than a preference, so sift-it would measure them:

- the prior median, the typical median of a query's scores, now 0.55 from the testbed
- the length penalty, which follows from the typical spread of a query's scores. `docs/ranking.md` derives its 0.0125
  from a spread of 0.03.

The engine would measure both when it loads the vectors, with a fixed set of probe queries. A collection of only a few
segments keeps the built-in values, and `sift-it status` shows what was measured.

### Smaller things

- Ignore patterns for keys and columns in structured files, like `**.id` or `created_at`, to keep identifiers and
  timestamps out of the passages.
- Images inside documents, place names from GPS positions, project summaries written by a language model.

## Open questions

- How the candidates a sketch search needs grow with an index past 10,000 vectors. Its best matches stand further above
  the rest as it grows, so the share it takes should fall, and a 32nd would keep up; a large real collection will
  tell. `sift-it eval --compare docs/demo-queries.json` measures the demo collection.
- What a project summary needs: which files, and how many commit messages.
- The chapter break threshold, tuned on recordings without chapter markers.
- How a word found only in metadata counts when the query has a number, which makes the words decide. "beach 2017"
  belongs in the eval set.
- Whether 10% of memory is the right default search budget.
