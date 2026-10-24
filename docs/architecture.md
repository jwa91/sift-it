# Architecture

How sift-it's code is structured: the rules it follows, its parts and their boundaries, and where each lives in the
source. `docs/design.md` says what sift-it does; this says where each part of that lives.

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
clients     CLI · TUI · serve · other front ends
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
  Markdown, code, records (CSV, JSON, YAML, TOML), pages, frames, sound windows, speech. Pure, and tested on its own.
- **Prepare:** a segment becomes the model's input: the prompt with the file's title, a picture fitted to its token
  budget, a window of sound.
- **Embed:** the model's only owner. It batches inputs and hands back vectors. It holds the text model, the vision
  encoder only while pictures or video frames are read, and the audio encoder only while sound is.
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

## Concrete parts

There are no traits. The embedder, the transcriber, the index, the config and the policies each exist once, and an
extractor is a function per container format, chosen by a match on the kind. The policies are tested as pure
functions, and the rest on real files: the testbed, the fixtures in `tests/`, and indexes in temporary folders. A trait
comes when something varies.

## Crates

Module boundaries first, with the rule that dependencies point inward. A crate of its own only where it pays:

- **`sift-it-protocol`:** shared with every client, small, no heavy dependencies.
- **`sift-it-llama`:** the embedder's adapter, with llama.cpp's native build and bindings, built apart so it doesn't
  slow down every other build.
- **`sift-it-transcribe`:** the speech helper, a binary of its own (`transcribe/`).

There is one `sift-it` binary: the CLI, with the services as its subcommands. Front ends talk to the engine through
the protocol, not by linking code.

## The modules

```
src/
  main.rs                the CLI: commands and arguments
  config/                the schema (mod.rs), which files (sources.rs), and the cascade of reading values (reading.rs)
  resources.rs           the Mac's facts; plans for search and ingest
  models.rs              the model files in the Hugging Face cache: found by their hash, and downloaded
  index/                 the facade: catalog (SQLite), vector files, the vectors as searches read them
  ingest/
    discover/            what's new or changed (mod.rs), crawling (crawl.rs), what reading costs (cost.rs)
    read.rs              extract, segment and prepare, joined
    extract/             per container: text, document, notebook, pdf, image, audio, video, speech
    segment/             per content type: text by its form (passages; prose, code, records), pictures, frames,
                         sound, speech
    prepare.rs           prompts, fitting pictures, sound
    embed.rs             the embedding stage, the model's owner: the text model, and the encoders while needed
    write.rs             files put together and written
    schedule.rs          order, readers, the run
    listen.rs            speech's lane, beside the reading or after it
  query/
    understand.rs        words, phrases, code, kinds
    retrieve.rs          full-text and vector candidates
    rank.rs              lift, penalties, bonuses, grouping
    present.rs           excerpts and places
  services/
    engine.rs            the query service and control plane
    indexer.rs           the ingest service: watching the folders (FSEvents), waiting, status
    launchd.rs           installing the LaunchAgents
  adapters/
    macos/               the frameworks: ImageIO, CoreGraphics and Vision (mod.rs), AVFoundation (av.rs), power
    speech.rs            the helper process
  clients/               the engine's client, the TUI, results in a terminal, previews, opening results, serve
  tools/                 eval, the benches, selftest, estimate
crates/
  protocol/              sift-it-protocol
  llama/                 sift-it-llama: the embedding model and its picture sizing, used as `embedder`
transcribe/              sift-it-transcribe
```

Watching the folders is the indexer service's: FSEvents, settled for a moment, is platform wiring, bound to its loop
of pausing and waiting. It hands discover the paths that changed.

Text is cut by one cutter (`segment/passages.rs`), with a form per content type saying where its text breaks: prose
and Markdown at paragraphs and headings, code at its definitions, records at their rows, records or keys.

## Not done on purpose

- No plugin system.
- No async runtime: threads and bounded channels suit work that waits on the CPU and the GPU.
- No second binary for the services, and no library for the front ends to link: they speak the protocol.
