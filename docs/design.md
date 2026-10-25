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
  indexer loads each for its round ([Indexing](#indexing)): the text model for text, with the vision encoder for
  pictures and with the audio encoder for sound, and the speech helper for speech. Both encoders are in one file, and
  each loads on its own (a switch sift-it adds to mtmd: `scripts/llama.cpp.patch`). An encoder goes when its round is
  over, and the speech helper when speech is.

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

### Rounds

It goes over the whole collection in rounds, each with one tool, so that what's cheap is searchable soon, and a model
loads once for everything that needs it rather than once per file:

| Round | Its tool, on its chip | Reads | So a file is found by |
|---|---|---|---|
| words | none: readers, on the CPU | every file | its name, tags and metadata, and the text that's simply in it: plain text, code, records, a PDF's text layer, documents through textutil |
| text | the text model, on the GPU | passages | what they mean |
| OCR | Vision's text recognition, on the Neural Engine | screenshots, scanned pages | the text on them |
| pictures | the text model with its vision encoder, on the GPU | photos, screenshots, scanned pages with little text, video frames | what they show |
| sound | the text model with its audio encoder, on the GPU | recordings, a video's sound | what they sound like |
| speech | Parakeet Ultra in the speech helper, on the Neural Engine | recordings, a video's sound | what's said in them |

- **A file's kind decides its rounds.** A note goes through words and text, a photo through words and pictures, a
  screenshot through words, OCR, pictures and text, a video through words, pictures, sound, speech and text. Its depth
  leaves out what it doesn't reach: at `words`, a file goes through words and OCR only. What a PDF needs is known once
  its words are read: OCR for its scanned pages, and pictures for the scanned pages with little text.
- **Speech before sound, for each file.** A file's sound round waits for its speech. Where speech was heard, the sound
  round embeds a window only every two minutes, since the passages say what's in it, and its sound adds little to
  them; music, nature and the stretches between what's said keep a window every 20 s. Speech is told from singing by
  its passages' words a second: half of them at least 1.8, where three podcasts and two narrated videos had 2.25 to
  3.39, and an aria whose singing was written down as garbled French and Italian 1.29. On three podcasts, 327 windows
  became 56. On a part of the demo folder (three podcasts, three pieces of music, five videos), indexing took 62 s
  instead of 113, and each query for the music found what it did before.
- **Words first, for every file.** They need no model, so an exact search finds everything within minutes of choosing
  the folders, while meaning comes round by round.
- **What OCR and speech find is words at once.** It goes into the keyword index as soon as it's read, and its passages
  join the text round like any others.
- **Cheapest first.** A round's queue goes from the file with the least to do in it to the one with the most.
- **An item at a time.** A round works in items: a file, a batch of 16 passages, ten pages of a scan, or five minutes
  of a video. What an item found is written when it ends, with where the next one goes on. A recording's sound is read
  in one go, and every five minutes what it found is written with how far it got: it stops there when the plan needs
  its lane. So a file stopped partway goes on from there, and what runs can change between any two items, or at such a
  checkpoint, without losing work.

### Tools and the budget

`indexing.memory_mb`, `auto` by default, is a ceiling on what the tools loaded at once hold, with what the readers
decode: it decides how much runs side by side, never what's written. At `auto` it's a quarter of the Mac's memory.

Each tool says which chip it runs on and what it holds at its peak, as measured (`src/resources.rs`):

| Tool | Chip | Holds |
|---|---|---|
| the text model | GPU | 338 MB, and its 296 MB file mapped |
| the vision encoder | GPU | 289 MB, beside the text model |
| the audio encoder | GPU | 321 MB, beside the text model |
| Vision's text recognition | Neural Engine | 256 MB, and 171 MB on the Neural Engine |
| the speech helper | Neural Engine | 340 MB, 607 MB on the Neural Engine, and 32 MB mapped |
| a reader | CPU | 128 MB |

What runs is a plan, made again whenever what it depends on changes. Rounds that have work are taken new files' first
([New files first](#new-files-first)), then those whose tools are loaded, then in order (words, text, OCR, pictures,
sound, speech), and each joins when its tools fit in the budget beside those of the rounds before it. Words always run,
and so does the first round that needs a tool, whatever the budget: indexing never stops for want of memory, it only
goes slower.

- **One chip, one round.** Text, pictures and sound share the text model and the GPU: two of them at once would share
  its time, so neither would finish sooner, and the earlier one should. So the GPU takes pictures or sound, one at a
  time, with its encoder loaded, and while passages wait for the text round, their batches go first. The media round
  gets the GPU between them: passages keep coming as OCR and speech read them.
- **The Neural Engine takes OCR and speech together.** Measured, they get more done side by side than one after the
  other (below).
- **A loaded tool works through its queue.** Rounds bring each other work: OCR and speech bring the text round
  passages as they read. Taken in order alone, text would push speech out whenever passages wait on a small budget,
  and speech text, every second or two: at 1024 MB, three podcasts started the speech helper 14 times and took 29 s
  instead of 24. Now the passages wait for speech to be done, and the helper starts once.
- **The text model stays while it fits** beside what runs, counted in the budget: loading it again takes only 0.2 s,
  but the other rounds bring it passages every second or two. When it doesn't fit, it goes. At 1200 MB with speech
  alone, the indexer held 261 to 374 MB beside the helper while the text model stayed (and its 296 MB file mapped);
  it holds 108 to 194 MB now.
- **Each lane that reads has a reader,** in the budget: the words round's, and the GPU's media round's. More read at
  once with what's left, two to four in all (a third of the cores).

On a big budget, that's every chip busy at once. On a small one, one tool works through its whole queue before the
next loads. On a Mac with ten cores:

| `memory_mb` | What runs side by side | With screenshots and scans to read too |
|---|---|---|
| 1024 | words and text, then one tool at a time | the same |
| 2048 | words, text and pictures, then speech beside them, then sound; one file read at once beside speech | speech waits for OCR; with no pictures or sound to do, OCR and speech go side by side and text waits for one of them |
| 4096, and `auto` from 16 GB | words, text, pictures and speech, then sound; three files read at once | everything |

### New files first

Files that appear or change while the indexer runs (FSEvents tells) take a fast lane: they go first in every round
they need, so a new screenshot is found by its words and by what it shows within seconds, even while a long video is
read. A chip on another round lets it in between two of that round's items, and since an item is at most five
minutes of sound or video, that's seconds. On the GPU, the other encoder goes and theirs loads (51 to 69 ms).

### Files only in iCloud

With Optimize Mac Storage, iCloud Drive keeps some files only in the cloud: their names, sizes, times and extended
attributes are on the Mac, their data isn't, and reading it downloads it. sift-it never downloads one. The processes
that read files have macOS fail such a read instead (`setiopolicy_np`, in every framework and in textutil), and
discover reads nothing of such a file but its metadata and its screenshot mark.

- A file never read is found by its name, with nothing of it read. Once it's downloaded (FSEvents tells), it's read
  like a new file.
- A file macOS removes the download of keeps what was read of it, and is found as before. Removing a download keeps
  the file's size and time, so when it's back, nothing is read again. Work it still had waits for its download (a
  work state of its own; the text round's goes on, since it needs only the index), and goes on from where it was.
- A file changed on another device while it's only in the cloud is found by its name until it's downloaded.
- Status counts the files that wait for their download (`cloud`), and `sift-it estimate` counts them apart. A preview
  of one fails. Opening one is someone's own request: the app it opens in downloads it, as from Finder.

### Memory pressure

The budget is a ceiling, and macOS says when memory runs short (a memory-pressure dispatch source):

- **Warning:** the plan shrinks to one tool, as on the smallest budget, with one reader for each lane that reads.
- **Critical:** everything stops at once, and lets its memory go: the models, the encoders, the speech helper. The
  items in hand are read again later. Indexing goes on once the pressure is back to a warning or less.

### Power

On battery, `indexing.on_battery` decides what goes on:

- `words`, the default: the words and OCR rounds (no model: every file found by its exact words, the text on
  screenshots and scans included) and new files, while the other rounds wait for a charger, and the old files' work in
  hand stops at its next checkpoint. Meaning costs energy: on the M5, above idle, a thousand passages take 408 J, a
  photo 1.9 J, a video frame 0.8 J, 30 s of sound 2.6 J, and an hour of speech 116 J. The text of a large collection
  can take more than a MacBook Air's whole battery (190 kJ).
- `all`: everything goes on.
- `none`: nothing does, until there's a charger.

Low Power Mode lets only words, OCR and new files go, or nothing at `none`. `sift-it index` doesn't wait: it was asked to
index now.

### How far it is

Each round knows how far it is: what's done of what's known, in its own unit (files, passages, pictures, seconds of
sound), and how fast it went while it ran, for the time it has left. There are no fixed rates: a fanless Mac slows as
it warms, and the rounds measure that as they go. So status can say "words: done, text: 60%, pictures: 10%, speech:
waiting", and the time left once a round has run a while. A round's total grows while the rounds before it find more:
the text round's passages, as words, OCR and speech read them.

Before a folder is indexed, `sift-it estimate` says what each round will have to do in it and about how long that
takes, at the depth each file is read, from rates measured on an M5 (`src/ingest/discover/cost.rs`). It looks into a
file as little as it can: a recording's or a video's length and whether it has sound, and four of a PDF's pages for
whether it's scanned. Then it runs the plan at `indexing.memory_mb` over that work, for how long all of it takes with
the rounds on different chips side by side. On the demo folder it says 2,027 s, and indexing took 2,037 s at 2048 MB
and 2,109 s at `auto`; for three of its podcasts it says 101 s, which took 94.

Measured on an M5 with 24 GB:

- **The demo folder:** 628 files (428 photos, 91 videos, 71 recordings, 38 documents and texts, 28 hours of sound),
  read with `sift-it index` while the engine searched by meaning every 5 s, file by file before and in rounds after.
  The peaks are each process's footprint, and the Neural Engine's memory after the plus.

  | `memory_mb` | | Every file found by its words | Text by meaning | All done | Indexer | Speech helper | Engine | All at once |
  |---|---|---|---|---|---|---|---|---|
  | 2048 | before | 2,171 s | 2,179 s | 2,844 s | 1,018 MB | 149 + 607 MB | 290 MB | |
  | | after | 5 s | 93 s | 2,037 s | 756 MB | 313 + 607 MB | 385 MB | 1,764 MB |
  | 4096 | before | 2,130 s | 2,140 s | 2,164 s | 1,107 MB | 116 + 607 MB | 316 MB | 2,123 MB |
  | | after¹ | 5 s | 104 s | 2,131 s | 822 MB | 336 + 607 MB | 309 MB | 1,934 MB |
  | `auto` (6 GB) | before | 2,117 s | 2,127 s | 2,151 s | 1,072 MB | 104 + 607 MB | 309 MB | 2,041 MB |
  | | after | 6 s | 104 s | 2,109 s | 767 MB | 342 + 607 MB | 506 MB | 1,843 MB |

  ¹ Measured with an earlier plan, in which the text round could push out a loaded tool and the words and media
  lanes each took all the readers. At 2048 MB and `auto`, the plan as it is now found every file by its words 5 s
  sooner and as soon, text by meaning 8 s and 3 s sooner, and was all done 38 s and 26 s sooner.

  Read file by file, a file was found only once all of it was read, and the cheapest went first: a book and the long
  podcasts came last, so nearly everything was found only at the end. All done is the GPU's work, which is the same
  either way; at 2048 MB speech was written down after the reading before, and beside it now. The indexer holds less
  because it never has both encoders loaded.

- **The same index.** The testbed indexed in rounds has the segments it had read file by file, all 578, with the same
  places and text, and its 115 vectors are the same (cosine 1.0). `make eval` and `make selftest` give what they did.
  On the demo, all segments but 36 passages of speech are the same, and those differ in a word or two: two runs file
  by file differ in 39, since the speech model doesn't write the same twice to the letter. Of the 21,931 vectors,
  the frames and pictures are the same to the bit, and the rest at a cosine of 0.99997 or more.
- **New files first.** In the daemon at 2048 MB, a screenshot saved while the GPU read a podcast's sound was found by
  its words 1.8 s after it landed (1.5 s of that is `indexing.quiet_ms`), by the text on it at 2.7 s, by that text's
  meaning at 3.3 s, and by what it shows at 6.3 s: the sound round went on to its next checkpoint, and the vision
  encoder took over.
- **An item at a time.** A podcast whose sound round was stopped at 600 s went on from there, and its 105 windows
  are those of one read, to the bit (cosine 1.0): a read that starts partway warms the decoder up first
  (docs/audio-decoding.md). A video's frames go on after the frame kept last, which the item before wrote down, and
  are the same too. Reading sound in five-minute items that each started partway wasn't: in one of the demo's VBR MP3s
  (music), AVFoundation's seek lands a fraction of a second off, and every window after the first seek was shifted
  (cosine 0.977). So a recording is read in one go, and only one that was stopped is read from partway.
- **OCR and speech on the Neural Engine.** The 362 pages of the demo's PDFs as screenshots, and ten of its podcasts
  (6.4 hours), at `auto`:

  | | side by side | one after the other |
  |---|---|---|
  | The Neural Engine alone (the GPU's rounds waiting) | 242 s | 286 s |
  | With the GPU's rounds as well (the podcasts' sound, the text round) | 347 s | 385 s |

  Alone, OCR takes 0.53 s a screenshot and speech goes 269 times as fast as the sound plays; side by side, 0.94 s
  and 249 times. OCR hardly minds the GPU being busy (0.57 s a screenshot beside `sift-it bench-embed`, 0.61 s beside
  another process reading sound, 0.64 s beside three busy cores).
- **One thread for the GPU, and the writer.** It embeds a sound window in about 200 ms, and the lanes that need no GPU
  can't wait that long for each message: the words, OCR and speech lanes are answered between any two of the media
  lane's pieces, and may run far ahead. When they were answered once a pass, OCR took 3 s a screenshot beside the
  sound round in the same process, speech went at half its speed, and the demo's words took 107 s instead of 9.

- The embedding model's batch size changes nothing: 17.7 passages a second one at a time, and 17.7, 18.1 and 18.0 in
  batches of 2,048, 4,096 and 8,192 tokens, with the same footprint. So it isn't part of the budget, and stays at
  2,048 tokens (an image takes 1,120). Whether a bigger GPU gains from bigger batches,
  `SIFT_IT_BATCH_TOKENS=4096 sift-it bench-embed` shows.
- Two readers index the testbed in 20 s, one in 29 s, at the same peak: what a reader decodes is small next to the
  model. The budget reckons with 128 MB a reader.

The indexer follows the folders with FSEvents and makes a full sync now and then, and lets the model go when there's
nothing to do.

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
- **Memory:** 100 to 120 MB of its own on long recordings, plus the Neural Engine's copy of the model, 607 MB, and the
  32 MB it maps of what Core ML compiled for the Neural Engine. That copy doesn't show in its footprint: `footprint`
  lists it apart, as "owned physical footprint (neural)", and proc_pid_rusage as `ri_neural_footprint`. The first time
  a Mac loads the model, the Neural Engine compiles it (about 8 s), and Core ML keeps that in
  `~/Library/Caches/sift-it-transcribe`. A run of short recordings, each a chunk of a length of its own, takes more:
  FluidAudio keeps a buffer of each for a while (`footprint`: large allocations of a chunk's size), and the demo's
  videos, cheapest first, took the helper to 245 to 336 MB before it let them go.
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

- **Waiting.** Each round goes from the cheapest file to the dearest, and words come first for every file
  ([Rounds](#rounds)). Most files are searchable soonest, and the podcast comes last, but whole. A file that just
  changed goes first.
- **Memory.** Readers stream: text in pieces, PDFs by page, sound and video by window. A big file's segments are
  written in batches as they come, like transcripts, so memory doesn't grow with the file. A search can meanwhile find
  the part that's in.
- **A signal.** `sift-it status` and the front ends show the work still queued, and the heaviest files with their
  estimate. The user can act on it, with `ignore` or a lower depth, or let it run.

### Kinds

Kinds are plaintext, document, structured, image, screenshot, audio, video, code and conversation. `sources` maps
file types to kinds, with built-in defaults. A PDF is a document. A screenshot is a picture macOS (or a screenshot
tool) marked as one: it's read for its text as well as what it shows, and takes the image settings. A conversation is
a JSON Lines file whose first lines are an agent's session log.

| Kind | Settings |
|---|---|
| image | `tokens`, and `text_tokens` for screenshots |
| audio | none beyond depth |
| video | shot change, keep every n seconds, sound on or off (no sound, no speech). Frames take `tokens` |
| document | `text_tokens` for scanned pages, which get OCR |
| plaintext | passage size, cut at headings |
| code | passage size, cut at definitions |
| structured | passage size, cut at records |
| conversation | passage size, cut at exchanges |

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

### Conversations

Claude Code keeps a log of each session in `~/.claude/projects/<folder>/<session>.jsonl`, and Codex in
`~/.codex/sessions/<year>/<month>/<day>/rollout-*.jsonl`: a JSON record per line. A log is known by its first lines
(Claude Code's records carry a `sessionId`, Codex's log starts with its `session_meta`), so one is found in any folder,
and a chosen folder may be hidden itself (only what's below it is judged).

- **What's read** is what the user said and what the agent answered. Not read: tool calls and what they gave back, the
  agent's thinking, the app's own records, what an app wraps around a prompt (an element on lines of its own, such as
  `<system-reminder>` or a command's output, unless it holds the user's own words: a command they typed, text they
  pasted), Codex's instructions, and a subagent's transcript (its conversation is the one that started it). What
  Claude Code keeps for a session in a folder beside its log (tool output, subagents' transcripts) isn't read either.
- **An exchange** is what the user says after the agent answered, and what the agent says until the user speaks again,
  with who speaks named where that changes (`User: …`, `Claude: …`). It's cut as Markdown: a short one is a passage, a
  long one several, each part after the first embedded with what was asked before it. A passage is at the time its
  first message was said, to its last one's.
- **Its title** is the one the app gave the session (Claude Code's custom title), else the agent's (its AI title, an
  older log's summary), else its first prompt. It's the result's name, it's in the keyword index with every passage,
  and the passages are embedded with it, as a document's with its file name.
- **A running session's log** grows every few seconds. It's handed over 30 s after it went quiet (the agent waits for
  the user), and its next read goes on from where its last exchange started, which may have grown: what was read before
  stays, and of the last exchange what's the same keeps its vector. A log that's shorter, or other just before that
  point, is read again from the top. On this Mac's 365 MB of logs, what's said is 0.35% of their bytes: 1,401 passages
  of 1.26 million characters, read in 0.27 s.
- **Resuming** a session: a result's `info` has the command (`claude --resume <session>`, `codex resume <session>`)
  and the folder to run it in. `sift-it open` resumes it there when it runs in a terminal, and says the command
  otherwise.

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
  "indexing": {"memory_mb": "auto", "on_battery": "words", "quiet_ms": 1500, "full_sync_min": 30, "release_model_s": 60},
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
- The Photos library.
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
