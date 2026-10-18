# sift-it

Local file search for macOS, written in Rust.

Search selected folders by meaning or exact words. Use the interactive terminal interface, run commands from scripts,
or search from another program through `sift-it serve`.
Search and indexing run on your Mac with [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2) and
[llama.cpp](https://github.com/ggml-org/llama.cpp).

sift-it builds on the design of [DigUp](https://github.com/ARahim3/DigUp), created by Abdur Rahim.
Its reference test data also comes from DigUp. Both projects support local search across documents, pictures,
recordings, videos, and code. DigUp also includes a CLI.

## How sift-it differs

- **Rust implementation:** the search engine, file readers, and background indexer use Rust.
- **Terminal interface:** search interactively, preview results, and open files at the matching page, time, or line.
- **Script integration:** use JSON output or pass search result paths to other commands.
- **One engine for every search:** a resident process answers the terminal, scripts, and other programs alike. It
  loads the model and maps the vectors for the first search by meaning and keeps them, and a search by words loads
  nothing.
- **A protocol for front ends:** other programs search, preview results, manage folders and follow indexing through
  `sift-it serve`, with JSON lines.
- **Ranking changes:** account for file length and give ordinary words less weight than numbers, code searches,
  and quoted phrases.
- **Text cut by its form:** Markdown at its headings, code at its definitions, and records (CSV, JSON, YAML, TOML)
  one by one, so a passage is about one thing, and a piece of a long one is read with its heading or signature.
- **Speech, written down:** what's said in recordings and videos is transcribed in the background, so a word or a name
  spoken in a podcast is found at its moment. With enough memory this runs beside the reading; otherwise only after
  everything else is searchable.

### Ranking for mixed files

A long document has more chances to contain an accidental match than a picture or short recording.
sift-it lowers its score according to the number of stored vectors. Files with up to four vectors receive no penalty.

Ordinary word matches receive a smaller bonus than searches containing numbers, code searches, and quoted phrases.
This reduces cases where incidental text matches outrank relevant pictures or recordings.
Ordinary word matches do not automatically qualify as strong results.

sift-it also limits each file to 10 matching segments before it applies overall candidate limits.
It filters file types before these limits. This prevents one long file from excluding other results.

The recorded evaluation used a mixed collection with eight books:

| Measure | Before ranking changes | After ranking changes |
|---|---:|---:|
| Correct file first | 70/107 | 82/107 |
| Book first for queries about other files | 21/95 | 1/95 |
| False alarms for queries without answers | 1/5 | 0/5 |
| Correct book first for book queries | 12/12 | 12/12 |

These results compare sift-it before and after its ranking changes. They do not compare sift-it against DigUp.
See [the ranking notes](docs/ranking.md) for the formulas, evaluation method, and remaining limitations.

## Install

Requirements: Apple Silicon and macOS 14 or later. The model download is 1.6 GB: 865 MB to search, and 740 MB to write
down what's said in recordings. If [Handy](https://handy.computer) has the same speech model, sift-it uses its copy.

```sh
brew install jwa91/tap/sift-it
sift-it model download
sift-it folders add ~/Documents ~/Desktop ~/Downloads
sift-it service install
sift-it search "the warranty for the washing machine"
```

The background indexer and the search engine start at login. The indexer updates the index when files change.
macOS can request access to Documents, Desktop, or Downloads. Allow access when macOS requests it.
You can also grant Full Disk Access in System Settings → Privacy & Security.

## Search

### Interactive terminal

Run `sift-it` without arguments to search as you type. The interface shows results beside a preview of the selected file.
Supported terminals show pictures, PDF pages, and video frames inline. These terminals include Ghostty, kitty, iTerm2,
and WezTerm. Recordings show a waveform with the matching time range highlighted.

| Key | Action |
|---|---|
| Enter | Open the selected result at its matching page, time, or line |
| Ctrl+R | Show the selected file in Finder |
| Ctrl+Y | Copy the selected file path |
| Tab | Filter results by file type |
| Esc | Exit |

### Commands and scripts

```sh
sift-it search "the diagram of the heating system"
sift-it open 1
sift-it search "annual maintenance" --kind document -n 10
sift-it search '"SERVICE-2026-042" maintenance'
sift-it search "the railway bridge at sunset" --json
sift-it search "annual maintenance" | while IFS= read -r result_path; do open "$result_path"; done
```

Quoted phrases require matching words together and in order. Matching ignores case and accents.
Piped searches print one path per line. JSON output includes scores and matching locations.

### The engine

Every search goes to one resident process, the engine. It starts at login, or with the first search when it isn't
running, and holds nothing until it's needed. A search is by `meaning` or by `words`:

- **By meaning**, the default, uses the model and the vectors. The first such search loads the model and maps the
  vectors, which takes a few seconds, and they stay for every search after it.
- **By words** uses only the full-text index. It needs no model and holds almost nothing in memory.

```sh
sift-it search "INV-2024-0113" --by words
sift-it config search.mode words
sift-it engine unload
```

`sift-it config search.mode` sets the default, and `--by` overrides it for one search. `sift-it engine unload` lets the
model and the vectors go until the next search by meaning, and `sift-it status` shows what the engine holds. A new
version of sift-it replaces a running engine of an older one at its first search.

The vectors are in files beside the index, and the engine maps them rather than copying them into memory: macOS can
drop their pages when it needs the memory, and reads them again for the next search. `search.vectors` sets which form a
search scans:

- `exact-f32` reads every vector as it was made, for exact scores.
- `exact-f16` reads every vector at half precision: half as much, with scores within a thousandth.
- `sketch` compares the query with a 96-byte sketch of every vector, a 32nd of its size, then scores the closest
  exactly: 4,096 of them, or a 32nd of the vectors in a larger index. On Rust's books (7,400 vectors) it keeps all of
  the exact top 20 for 45 queries.
- `auto`, the default, scans the most precise form whose rows fit in `search.memory_mb`. That's a tenth of the Mac's
  memory unless you set it. 800 MB holds exact scores for some 260,000 vectors.

Every form is always written, so a change takes effect at the next search. A result's lift is measured from the
query's median over a fixed sample of 8,192 vectors, which the engine keeps in memory (24 MB), so it's the same
whichever form a search scans.
`sift-it open <n>` opens a result from the last search.
PDF results open in Preview at their page. Recordings and videos open in QuickTime at their matching time.
Text and code results open at their matching line in a supported editor.

Enable code indexing before a code search:

```sh
sift-it config sources.code true
sift-it index
sift-it search "code: validate a configuration file"
```

Code appears in `code:` searches or when you select the code filter.
`NO_COLOR` disables colour. `SIFT_IT_IMAGES=0` disables inline pictures.

### Other programs

Other programs can use `sift-it serve`. It accepts JSON requests through stdin and returns JSON replies through stdout,
relayed to the engine over its socket, and it renders previews itself.
See [docs/protocol.md](docs/protocol.md) for the requests, replies and events.

## Supported files

sift-it reads only the folders you select. It does not change, move, or upload your source files.

| File type | Search content |
|---|---|
| Documents | Text from txt, md, rtf, doc, docx, odt, and html files, Markdown cut at its headings |
| PDFs | Individual pages, including text recognition for scanned pages |
| Pictures | Image content in formats macOS can read, including HEIC and RAW |
| Screenshots | Image content and recognized text |
| Recordings | Overlapping 30-second windows, and what's said in them, with results at the matching time |
| Videos | Frames from individual shots and long shots, plus audio windows and what's said |
| Tags | The title, artist and album of recordings and videos, found by their words |
| Records | Rows of csv and tsv files, records of json and jsonl files, keys of yaml and toml files |
| Code and notebooks | Definitions and notebook cells, with code indexing enabled |

Text that contains mostly numbers uses word search only. Silent audio windows do not enter the index.
The embedding model can miss or confuse sounds and pictures. See [the documented limitations](docs/ranking.md#results).

### What's said in recordings

Once everything is indexed, the indexer writes down what's said in recordings and videos with
[Parakeet Ultra](https://huggingface.co/moondream/parakeet-ultra), which understands 25 European languages, Dutch and
English among them. It runs on [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) in a helper process,
`sift-it-transcribe`, at a lower priority than everything else, and gives its memory back when it's done. It writes a
recording down a few minutes at a time: any change in your folders comes first, and it goes on where it stopped.
On an Apple silicon Mac it writes down a 30-minute podcast in under half a minute.

Each passage is a sentence or a few, with its time: a search for a word said in a podcast opens it at that moment.
`sift-it status` shows how far it is; `sift-it config reading.depth meaning` turns it off.

### How deep files are read

`reading.depth` decides how deep a file is read, in three stages that build on each other:

| Depth | A file is found by |
|---|---|
| `words` | its name, its tags and the words in it, including text on screenshots and scanned pages. No model needed |
| `meaning` | also what it means: passages, pictures, video frames and sound |
| `speech` | also what's said in it, written down (the default) |

There are no size or length limits: a four-hour podcast is read as fully as a short memo. The indexer reads the
cheapest files first, so most of a folder is searchable soon, and writes a big file in parts as it reads it.
`sift-it status` shows what's still waiting and the files that take longest. Leave those out with `sources.ignore`,
give them a lower depth, or let them run.

sift-it skips hidden folders, nested `~/Library` folders, app bundles, packages, dependency folders, and build folders.
It also skips recognized secret files and iCloud files that exist only in the cloud.
Inside code projects, it reads pictures, PDFs, and recordings unless you enable code indexing.
The indexer pauses in Low Power Mode. You can configure it to wait for a charger.

## Manage the index

```sh
sift-it folders add ~/Documents
sift-it folders exclude ~/Documents/Private
sift-it estimate ~/Documents
sift-it status --json
sift-it service status
sift-it index
sift-it model status
sift-it selftest
```

Run `sift-it --help` for all commands and options.

Data lives in `~/Library/Application Support/Sift It`, or the folder specified by `SIFT_IT_HOME`.
This folder contains `config.json`, `index.sqlite`, `vectors/`, `status.json`, `indexer.log`, `engine.log`,
`engine.sock`, `models/`, and `previews/`.
When the folder's path is too long for a socket's name (104 bytes on macOS), the engine's socket is reached through a
link to the folder in your temporary folder, `sift-it-<hash>`.
`SIFT_IT_SPEECH_MODEL` points at another copy of the speech model.
`brew uninstall --zap sift-it` removes the installation and its data.

### Configuration

`config.json` holds every setting, each with its value written out. `sift-it config` shows it, and
`sift-it config <key> <value>` sets a value by its dotted name. `null` puts a value back to its default.

```sh
sift-it config sources.ignore '["*.psd", "~/Documents/Old"]'
sift-it config reading.video.keep_every_s 10
sift-it config reading.rules '[{"path": "~/Movies/Footage/**", "depth": "words"}]'
sift-it config indexing.on_battery false
sift-it config indexing.memory_mb 4096
```

| Section | Holds | A change counts |
|---|---|---|
| `sources` | folders, ignore patterns, the file types of each kind, whether code is read | at the next sync |
| `reading` | how deep and how files are read, per kind, file type, and folder | the files it affects are read again |
| `ranking` | the constants search ranks by | at the next search |
| `search` | whether a search is by meaning or by words, unless it says, which form of the vectors it scans, and the memory that may take | at the next search |
| `indexing` | the memory, battery and timing of the background indexer | at once |
| `log` | the size of the indexer's log | at once |

`indexing.memory_mb` decides only how fast indexing goes, never what it writes. At `auto` it's a quarter of the
Mac's memory. With room for the embedding model and the speech model together (about 1 GB each, plus 256 MB for two
readers), what's said in recordings is written down beside the reading; with less, the models take turns. Reading
takes up to a third of the cores, two to four files at once, and a small budget makes that fewer. `sift-it status` shows the plan.

A reading value resolves in four steps, and the last one that sets it wins: the top of `reading`, the kind
(`reading.video`), the file type (`reading.types.mov`), then each rule in `reading.rules` whose path matches.
Ignore patterns and rule paths match a file or folder name anywhere (`*.psd`), or a path when they contain a slash
(`~/Documents/Old`, `**/Exports/**`). A folder that matches counts for everything in it.
See [docs/design.md](docs/design.md) for where the configuration is going.

## Build from source

Install Xcode's command line tools, Rust, and CMake before you build.

```sh
brew install cmake
git clone https://github.com/jwa91/sift-it
cd sift-it
make build
make check
make install
```

`make build` builds the pinned llama.cpp release and the sift-it binary.
`make check` runs formatting checks, Clippy, and tests. `make install` installs sift-it into `~/.local/bin`.

[docs/design.md](docs/design.md) says what sift-it does and the order it's built in.
[docs/architecture.md](docs/architecture.md) says how the code is structured: its parts, their boundaries, and which
way dependencies point. [docs/protocol.md](docs/protocol.md) is what front ends say to the engine.

After you download the model, `make selftest` checks output against the reference vectors from DigUp.
`make eval` builds `testbed/` inside this repository and measures search quality on it.
The mixed-file evaluation needs a larger demo collection, which isn't part of the repository;
[docs/ranking.md](docs/ranking.md) says how to make it.

## Credits and license

- [DigUp](https://github.com/ARahim3/DigUp), by Abdur Rahim, provides the original project design and reference test data.
  See [tests/reference/README.md](tests/reference/README.md) for the copied test files and their source.
- [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), by Google DeepMind, provides the embedding model
  under Apache 2.0.
- [ggml-org's GGUF conversion](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF) provides the model files used here.
- [llama.cpp](https://github.com/ggml-org/llama.cpp) provides model execution under the MIT License.
- [Parakeet Ultra](https://huggingface.co/moondream/parakeet-ultra), by Moondream (post-trained from NVIDIA's
  Parakeet TDT 0.6B v3), writes down speech under CC-BY-4.0, in
  [Handy's GGUF conversion](https://huggingface.co/handy-computer/parakeet-ultra-gguf).
- [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), by the Handy team, runs it under the MIT License.

sift-it uses the [MIT License](LICENSE). The license preserves DigUp's copyright notice alongside the sift-it copyright notice.
The copied reference files also include [DigUp's original MIT notice](tests/reference/LICENSE).
Gemma is a trademark of Google LLC.
