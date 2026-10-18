# Architecture

How sift-it's code is structured: the rules it follows, its parts and their boundaries, how today's code maps onto
them, and the order in which it gets there. `docs/design.md` says what sift-it does; this says where each part of that
lives. Nothing here changes behavior: every step on the way keeps `make check` and `make eval` as they were.

## Rules

1. **Dependencies point inward.** The domain is in the middle: how files are cut, how results rank, what memory allows.
   Technology is at the edge: llama.cpp, SQLite, Apple's frameworks, launchd. The domain knows the edge only through
   narrow interfaces.
2. **Writing and reading are two paths.** Ingesting files and answering searches share the index and the domain's
   types, nothing else.
3. **Each stage does one thing.** Typed data goes between stages, not shared state.
4. **A scarce resource has one owner.** One owner of the model (the GPU), one writer of the index.
5. **Policy is apart from mechanism.** The rules for cutting, ranking and memory are pure functions, tested without a
   model or a database, and fed by the config.
6. **Processes are thin.** The engine and the indexer wire parts together; they hold no logic of their own.
7. **One contract faces outward: the protocol.** Every client uses it, and only it.

## The model

```
clients     CLI · TUI · serve · web gateway · Mac app
                     │  protocol: requests, responses, events
services    engine: search and control · indexer: ingest
                     │
domain      query path:   understand → retrieve → rank → present
            ingest path:  discover → extract → segment → prepare → embed → write
            policy:       ranking · segmenting · resources · config
                     │  interfaces
adapters    index (SQLite and vector files) · embedder (llama.cpp) · transcriber (helper process)
            extractors (PDFKit, ImageIO, Vision, AVFoundation, textutil) · platform (FSEvents, launchd, power)
```

## Parts

### The ingest path

- **Discover:** which files are new or changed (by their reading fingerprint), and what reading each will cost.
- **Extract:** a file becomes a stream of content: lines of text, pages, pictures, sound. Extracting decodes; it decides
  nothing about cutting.
- **Segment:** content becomes segments, each with its place in the file. One strategy per content type: prose and
  Markdown, code, records (CSV, JSON, YAML), pages, frames, sound windows, speech. Pure, and tested on its own.
- **Prepare:** a segment becomes the model's input: the prompt with the file's title, a picture fitted to its token
  budget, a window of sound.
- **Embed:** the model's only owner. It batches inputs and hands back vectors.
- **Write:** the index's only writer. It keeps the rule that a segment's vectors are appended before the segment is
  committed.
- **Schedule:** what goes first (changes, then the cheapest), how many files are read at once, and whether speech is
  written down beside the reading or after it.

### The query path

- **Understand:** a request becomes words, phrases, a kind filter and a mode. Pure.
- **Retrieve:** candidates from two sources: the full-text index, and the vector index in the form the plan says
  (exact, half precision or sketches).
- **Rank:** a pure function from candidates and the query's median to results per file: lift, length penalty, word
  bonus, grouping, strong or weak.
- **Present:** the excerpt, and the line or second a result opens at.

### Shared

- **Index:** one facade over the catalog (SQLite: files, segments, full-text, transcripts, the queue) and the vector
  store (partitions, generations, forms). It has a writer (one) and readers (many). Snapshots, generations and mapped
  files stay behind it.
- **Resources:** the Mac's facts (memory, cores, power) go in, plans come out: the form a search scans and its
  candidates, the readers and speech of ingest. Pure functions, in one place.
- **Config:** the schema and the cascade. Each part gets its own typed section, not the whole config.
- **Protocol:** the requests, responses and events every client uses (`crates/protocol`, `docs/protocol.md`). Its
  first event carries the engine's build, so a client of another build notices; nothing is negotiated. The engine is
  the one place a client talks to, for searching and for control: the indexer's progress, the settings, the folders.

### Services

- **Engine:** the query path and the control plane, behind the protocol, in a LaunchAgent of its own.
- **Indexer:** the ingest path, in a LaunchAgent of its own, so a crash in a reader can't take search down.
- **Speech helper:** a process of its own because transcribe.cpp brings its own ggml. To the domain it's just a
  transcriber.

## Kind is a filter, not the structure

A kind (picture, video, document) is what a user filters by. It isn't the axis to cut the code along. A video is a
container: frames (the picture logic), a sound track (the sound logic), and what's said (the transcriber). A PDF holds
text and page images, a screenshot a picture and its text. So extracting is per container format, and segmenting and
preparing are per content type; an extractor combines them. The picture logic exists once, also when a video uses it.

Speech is an extractor too: sound in, text with times out. A slow one, so the scheduler runs it on its own lane. What
it yields is segmented and embedded like everything else, and "beside the reading or after it" is the scheduler's
choice rather than code of its own.

## Interfaces

Only where there's variation or a test needs a stand-in:

- `Embedder`: llama.cpp, or a stand-in that makes vectors from a seed in tests.
- `Transcriber`: the helper process, or a stand-in.
- `Extractor`: one per container format.

The index, the config and the policies are concrete: there's one of each, and an interface would only hide them.

## Crates

Module boundaries first, with the rule that dependencies point inward. A crate of its own only where it pays:

- **`sift-it-protocol`:** shared with every client, small, no heavy dependencies.
- **`sift-it-llama`:** the embedder's adapter, with llama.cpp's native build and bindings. Built apart, it no longer
  slows down every other build.
- **`sift-it-transcribe`:** the speech helper, as today.

It stays one `sift-it` binary: the CLI, and the services as its subcommands. The web UI and the Mac app talk to the
engine through the protocol, not by linking code, as sibling tools in the family do.

## The modules

```
src/
  main.rs                the CLI: commands and arguments
  config/                the schema and the cascade
  resources.rs           the Mac's facts; plans for search and ingest
  index/                 the facade: catalog (SQLite), vectors (files, generations, scanning)
  ingest/
    discover.rs          crawling, watching, fingerprints, cost
    extract/             per container: text, document, pdf, image, audio, video, speech
    segment/             per content type: prose, markdown, code, records, pages, frames, sound, speech
    prepare.rs           prompts, fitting pictures, sound windows
    embed.rs             the embedding stage
    write.rs             the writer
    schedule.rs          order, readers, speech's lane
  query/
    understand.rs        words, phrases, code, kinds
    retrieve.rs          full-text and vector candidates
    rank.rs              lift, penalties, bonuses, grouping
    present.rs           excerpts and places
  services/
    engine.rs            the query service and control plane
    indexer.rs           the ingest service
    launchd.rs           installing the LaunchAgents
  adapters/
    macos/               the frameworks: AVFoundation, ImageIO, PDFKit, Vision, textutil, FSEvents, power
    speech.rs            the helper process
  clients/               the TUI, serve (relay and previews), opening results, the engine's client
  tools/                 eval, the benches, selftest
crates/
  protocol/              sift-it-protocol
  llama/                 sift-it-llama
transcribe/              sift-it-transcribe
```

## Where today's code stands

| Part | Today | What's off |
|---|---|---|
| Discover | `crawl.rs`, `estimate.rs` | close |
| Extract and segment | `ingest/extract/`, `ingest/segment/`, `ingest/prepare.rs` | done (step 4 below) |
| Embed | `embedder.rs` | close; the model's owner is the indexer's loop |
| Speech | `speech.rs`, `transcribe/` | a stage of its own beside the pipeline, not an extractor |
| Write and schedule | `indexer.rs`, `daemon.rs` | ordering, the pipeline, speech and storage in one loop |
| Index | `index/` | done (step 3 below) |
| Query | `query/` | done (step 2 below) |
| Resources | `config.rs`, `indexer::Plan`, `query/mod.rs`, `mac.rs` | four places |
| Protocol | `crates/protocol`, `docs/protocol.md` | done (step 1 below) |
| Clients | `finder.rs`, `term.rs`, `present.rs`, `preview.rs`, `open.rs`, `serve.rs` | beside the core, in one namespace |

## The way there

Each step changes no behavior, and is checked by `make check` and `make eval` giving what they gave before. A step
that touches segmenting also keeps every segment, and so every fingerprint and vector, as it was.

1. **The protocol:** its types in `sift-it-protocol`, documented. The engine reports the indexer's progress and takes
   settings and folders through it.
2. **The query path:** understand, retrieve, rank and present as modules; ranking pure.
3. **The index:** one facade with a writer and readers over the catalog and the vectors.
4. **Extracting apart from segmenting:** extractors yield content, segmenters cut it.
5. **The ingest path:** stages with owners, a scheduler, and speech as an extractor on a lane of its own.
6. **Resources:** the plans in one module.
7. **Crates:** `sift-it-llama`, and the modules in the tree above.

Then segmenting text by its form lands where it belongs: prose and Markdown at paragraphs and headings, code at its
definitions, records at whole rows, records or top-level keys.

## Not done on purpose

- No interface for every part: only where something varies or a test needs a stand-in.
- No plugin system.
- No async runtime: threads and bounded channels suit work that waits on the CPU and the GPU.
- No second binary for the services, and no library for the front ends to link: they speak the protocol.
