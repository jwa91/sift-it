# sift-it

Local file search for the terminal and Raycast, written in Rust.

Search selected folders by meaning or exact words. Use the interactive terminal interface, run commands from scripts,
or search through [Sift It for Raycast](https://github.com/jwa91/sift-it-for-raycast).
Search and indexing run on your Mac with [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2) and
[llama.cpp](https://github.com/ggml-org/llama.cpp).

sift-it builds on the design of [DigUp](https://github.com/ARahim3/DigUp), created by Abdur Rahim.
Its reference test data also comes from DigUp. Both projects support local search across documents, pictures,
recordings, videos, and code. DigUp also includes a CLI.

## How sift-it differs

- **Rust implementation:** the search engine, file readers, and background indexer use Rust.
- **Terminal interface:** search interactively, preview results, and open files at the matching page, time, or line.
- **Script integration:** use JSON output or pass search result paths to other commands.
- **Raycast integration:** a separate TypeScript extension uses `sift-it serve` for searches and previews.
- **Ranking changes:** account for file length and give ordinary words less weight than numbers, code searches,
  and quoted phrases.

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

Requirements: Apple Silicon and macOS 14 or later. The model download is 865 MB.

```sh
brew install jwa91/tap/sift-it
sift-it model download
sift-it folders add ~/Documents ~/Desktop ~/Downloads
sift-it service install
sift-it search "the warranty for the washing machine"
```

The background indexer starts at login and updates the index when files change.
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
sift-it search "annual maintenance" --kind pdf -n 10
sift-it search '"SERVICE-2026-042" maintenance'
sift-it search "the railway bridge at sunset" --json
sift-it search "annual maintenance" | while IFS= read -r result_path; do open "$result_path"; done
```

Quoted phrases require matching words together and in order. Matching ignores case and accents.
Piped searches print one path per line. JSON output includes scores and matching locations.
`sift-it open <n>` opens a result from the last search.
PDF results open in Preview at their page. Recordings and videos open in QuickTime at their matching time.
Text and code results open at their matching line in a supported editor.

Enable code indexing before a code search:

```sh
sift-it config include-code true
sift-it index
sift-it search "code: validate a configuration file"
```

Code appears in `code:` searches or when you select the code filter.
`NO_COLOR` disables colour. `SIFT_IT_IMAGES=0` disables inline pictures.

### Raycast and other interfaces

[Sift It for Raycast](https://github.com/jwa91/sift-it-for-raycast) provides search, folder management, and indexing status.
The extension uses the same sift-it index and model.

Other programs can use `sift-it serve`. It accepts JSON requests through stdin and returns JSON replies through stdout.
It keeps vectors and the query encoder in memory while the interface remains open.
See [src/serve.rs](src/serve.rs) for the request and reply formats.

## Supported files and limits

sift-it reads only the folders you select. It does not change, move, or upload your source files.

| File type | Search content |
|---|---|
| Documents | Text from txt, md, rtf, doc, docx, odt, and html files |
| PDFs | Individual pages, including text recognition for scanned pages |
| Pictures | Image content in formats macOS can read, including HEIC and RAW |
| Screenshots | Image content and recognized text |
| Recordings | Overlapping 30-second windows, with results at the matching time |
| Videos | Frames from individual shots and long shots, plus audio windows |
| Code and notebooks | Source passages and notebook cells, with code indexing enabled |

Text that contains mostly numbers uses word search only. Silent audio windows do not enter the index.
The embedding model can miss or confuse sounds and pictures. See [the documented limitations](docs/ranking.md#results).

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
This folder contains `config.json`, `index.sqlite`, `status.json`, `indexer.log`, `models/`, and `previews/`.
`brew uninstall --zap sift-it` removes the installation and its data.

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

After you download the model, `make selftest` checks output against the reference vectors from DigUp.
`make eval` builds a test collection and measures search quality.
The mixed-file evaluation requires a separate demo collection. [docs/ranking.md](docs/ranking.md) provides reproduction commands.

## Credits and license

- [DigUp](https://github.com/ARahim3/DigUp), by Abdur Rahim, provides the original project design and reference test data.
  See [tests/reference/README.md](tests/reference/README.md) for the copied test files and their source.
- [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), by Google DeepMind, provides the embedding model
  under Apache 2.0.
- [ggml-org's GGUF conversion](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF) provides the model files used here.
- [llama.cpp](https://github.com/ggml-org/llama.cpp) provides model execution under the MIT License.

sift-it uses the [MIT License](LICENSE). The license preserves DigUp's copyright notice alongside the sift-it copyright notice.
The copied reference files also include [DigUp's original MIT notice](tests/reference/LICENSE).
Gemma is a trademark of Google LLC.
