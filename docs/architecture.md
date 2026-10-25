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
            ingest path:  discover → rounds: words · text · OCR · pictures · sound · speech
                          each: extract → segment → prepare → embed → write
            policy:       ranking · segmenting · rounds · resources · config
                     │  interfaces
adapters    index (SQLite and vector files) · embedder (llama.cpp) · transcriber (helper process)
            extractors (PDFKit, ImageIO, Vision, AVFoundation, textutil)
            platform (FSEvents, launchd, power, memory pressure)
```

## Parts

### The ingest path

- **Discover:** which files are new or changed (by their reading fingerprint), queued for the words round.
- **Rounds:** the policy of what indexing does, and in what order: the six rounds, the unit each counts in, which a
  file of each kind passes through, and their order. Pure, and tested on its own.
- **Extract:** a file becomes a stream of content, for one round at a time: its text and metadata (words), the text on
  its pictures (OCR), its pictures and frames, its sound, what's said in it (speech). Extracting decodes; it decides
  nothing about cutting.
- **Segment:** content becomes segments, each with its place in the file. One strategy per content type: prose and
  Markdown, code, records (CSV, JSON, YAML, TOML), pages, frames, sound windows, speech. Pure, and tested on its own.
- **Prepare:** a segment becomes the model's input: the prompt with the file's title, a picture fitted to its token
  budget, a window of sound.
- **Embed:** the model's only owner. It batches texts and hands back vectors. It holds the text model while any of
  the GPU's rounds has work, and one encoder: the vision encoder for pictures, the audio encoder for sound.
- **Write:** the index's only writer. Each round writes only its own segments (words its passages and pages, OCR its
  passages and the words on pictures, pictures its pictures and frames, sound its windows, speech its passages), and
  the text round the vectors of the passages waiting for one. It keeps the rule that a vector is appended before the
  segment that points at it is committed.
- **Schedule:** the run. It asks resources for a plan (which rounds run side by side), hands their items to the lanes,
  lets new files in first, and keeps each round's progress.

The queues are the index's: a row per file and round it needs (`work`), with its state, how much there is to do, how
much is done, and where the next item goes on; a passage waiting for its vector says so (`segments.due`). The
scheduler only holds the few items it hands out next, so a stop or a crash loses at most the items in hand.

A run has a thread for each lane, and items go between them on bounded channels:

- **The run's own thread:** the plan, the GPU (embed), the writer, and what the other lanes hand back.
- **The words lane:** a few readers, reading files' words.
- **The media lane:** as many, decoding for the GPU's round: pictures and frames, or sound.
- **The OCR lane:** one thread, since Vision reads text in one picture at a time per process anyway.
- **The speech lane:** one thread, which talks to the speech helper.

The run's thread embeds a picture in about 120 ms and a sound window in 200 ms, and a batch of the text round's
passages in a few hundred. What the other lanes hand back needs no GPU, and comes in many small messages (a passage, a
page's text, a stretch of what's said), so it's taken between any two of the media lane's pieces, and those lanes may
run far ahead: taken once a pass, each message waited for a picture's embedding, and OCR went six times slower beside
the sound round, speech half as fast, and words crawled. The speech lane doesn't wait for a stretch to be stored at
all: it goes on with the next, with the next file waiting beside it, and the run stops a file by naming it. On the
GPU, the text round's batches go first while passages wait (unless the media round's work is new files'), and the
media lane's pieces wait meanwhile.

### The query path

- **Understand:** a request becomes words, phrases, a kind filter and a mode. Pure.
- **Retrieve:** candidates from two sources: the full-text index, and the vector index in the form the plan says
  (exact, half precision or sketches).
- **Rank:** a pure function from candidates and the query's median to results per file: lift, length penalty, word
  bonus, grouping, strong or weak.
- **Present:** the excerpt, and the line or second a result opens at.

### Shared

- **Index:** one facade over the catalog (SQLite: files, segments, full-text, the rounds' queues) and the vector
  store (partitions, generations, forms). It has a writer (one) and readers (many). Snapshots, generations and mapped
  files stay behind it.
- **Resources:** the Mac's facts (memory, cores, power, memory pressure) go in, plans come out: the form a search
  scans and its candidates, and for ingest which rounds run side by side and how many readers. Each tool declares its
  chip and what it holds, so a tool that isn't built yet (a Core ML text model on the Neural Engine, say) slots in by
  saying so. Pure functions, in one place.
- **Config:** the schema and the cascade. Each part gets its own typed section, not the whole config.
- **Protocol:** the requests, responses and events every client uses (`crates/protocol`, `docs/protocol.md`). Its
  first event carries the engine's build, so a client of another build notices; nothing is negotiated. The engine is
  the one place a client talks to, for searching and for control: the indexer's progress, the settings, the folders.

### Services

- **Engine:** the query path and the control plane, behind the protocol, in a LaunchAgent of its own.
- **Indexer:** the ingest path, in a LaunchAgent of its own, so a crash in a reader can't take search down.
- **Speech helper:** a process of its own, because it's Swift (FluidAudio runs the speech model on the Neural Engine
  through Core ML), and because it gives all of its memory back when there's nothing to write down. To the domain it's
  just a transcriber: sound in, words with their times out.

## Kind is a filter, not the structure

A kind (picture, video, document) is what a user filters by. It isn't the axis to cut the code along. A video is a
container: frames (the picture logic), a sound track (the sound logic), and what's said (the transcriber). A PDF holds
text and page images, a screenshot a picture and its text. So extracting is per container format, and segmenting and
preparing are per content type; an extractor combines them. The picture logic exists once, also when a video uses it.

Speech is an extractor too: sound in, words with times out, which the speech segmenter cuts into passages. A slow one,
on the Neural Engine, so it's a round of its own. What it yields is cut and embedded like everything else, and whether
it runs beside other rounds is the plan's choice rather than code of its own.

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
- **`transcribe/`:** the speech helper, `sift-it-transcribe`: a Swift package beside the crates, a binary of its own.

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
    rounds.rs            the rounds: their order and units, and which a file passes through
    discover/            what's new or changed (mod.rs), crawling (crawl.rs), what reading costs (cost.rs)
    read.rs              extract, segment and prepare, joined, for one round
    extract/             per container: text, document, notebook, pdf, image, audio, video, speech
    segment/             per content type: text by its form (passages; prose, code, records), pictures, frames,
                         sound, speech
    prepare.rs           prompts, fitting pictures, sound
    embed.rs             the embedding stage, the model's owner: the text model, and an encoder while needed
    write.rs             what a round found, put together and written
    schedule.rs          the run: the plan, the lanes, new files first, progress
    progress.rs          how far each round is, and how fast it went
    listen.rs            the speech lane
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
    macos/               the frameworks: ImageIO, CoreGraphics and Vision (mod.rs), AVFoundation (av.rs), power,
                         memory pressure (pressure.rs)
    speech.rs            the helper process
  clients/               the engine's client, the TUI, results in a terminal, previews, opening results, serve
  tools/                 eval, the benches, selftest, estimate
crates/
  protocol/              sift-it-protocol
  llama/                 sift-it-llama: the embedding model and its picture sizing, used as `embedder`
transcribe/              sift-it-transcribe, in Swift
```

Watching the folders is the indexer service's: FSEvents, settled for a moment, is platform wiring, bound to its loop
of pausing and waiting. It hands discover the paths that changed.

Text is cut by one cutter (`segment/passages.rs`), with a form per content type saying where its text breaks: prose
and Markdown at paragraphs and headings, code at its definitions, records at their rows, records or keys.

## Not done on purpose

- No plugin system.
- No async runtime: threads and bounded channels suit work that waits on the CPU and the GPU.
- No second binary for the services, and no library for the front ends to link: they speak the protocol.
