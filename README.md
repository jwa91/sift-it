# sift-it

**A fast local search engine for the Mac, written in Rust.**

Find any file by its words, by what it means, or by what's said in it: the page of a PDF, the photo, the minute of a
podcast, the scene in a video. Everything runs on your Mac, and nothing you index or search leaves it.

This is the engine: the indexer, the search, and the `sift-it` command. [Sift It](https://github.com/jwa91/sift-it-mac),
the Mac app, is its front end, and other programs talk to it through [a documented protocol](docs/protocol.md).

## Install

```sh
brew install jwa91/tap/sift-it
```

That installs Sift It, the Mac app, with the engine inside it and the `sift-it` command on your path. Open Sift It: it
downloads the models, asks which folders to search, and starts indexing. Or, in a terminal:

```sh
sift-it model download
sift-it folders add ~/Documents
sift-it service install
sift-it search "the warranty for the washing machine"
```

Apple silicon and macOS 14 or later. The models are 1.5 GB, downloaded once.

## Three phases

Indexing comes in three phases. Each makes more of your files findable, and you can search all along.

| Phase | Finds | With |
|---|---|---|
| **Exact** | files by their name and exact words, text in screenshots and scans included | sift-it's own readers, and macOS's text recognition |
| **Embeddings** | documents, photos, sounds and video scenes by what they mean | [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), on [llama.cpp](https://github.com/ggml-org/llama.cpp) with Metal |
| **Transcribed** | what's said in audio and video | [Parakeet Ultra](https://huggingface.co/moondream/parakeet-ultra), on the Neural Engine through [FluidAudio](https://github.com/FluidInference/FluidAudio), in `sift-it-transcribe` |

Exact needs no model, so every file in a folder is found by its words within seconds of adding it. A file you just
saved goes first in every phase. Each phase is made of rounds, one tool each, which run side by side on the CPU, the
GPU and the Neural Engine as memory allows; `sift-it status` shows how far each one is.

Parakeet Ultra understands 25 European languages, English and Dutch among them. On an Apple M5 it writes down a
35-minute podcast in under ten seconds, and each passage it writes keeps its time, so a word said in a podcast opens
the podcast at that moment.

## How it runs

- **One engine.** Every search goes to one resident process, over a socket in the data folder: the terminal, scripts,
  the Mac app and other front ends alike. It holds nothing until it's needed. A search by words loads nothing; the
  first search by meaning loads the text model and maps the vectors, which stay until `sift-it engine unload`.
- **A background indexer.** It starts at login, follows your folders as they change, and lets its models go when
  there's nothing to do.
- **On battery,** Exact goes on and the rest waits for a charger, except for new files, which go through every phase.
  `indexing.on_battery` sets this: `words` (Exact only, the default), `all`, or `none`. In Low Power Mode it's Exact
  at most.
- **Within a memory budget.** `indexing.memory_mb`, a quarter of the Mac's memory unless you set it, decides how much
  runs side by side, never what's written. When macOS runs short of memory, indexing slows down or stops until it
  isn't.
- **Models shared with other apps.** They live in the Hugging Face cache (`~/.cache/huggingface/hub`, or
  `$HF_HUB_CACHE`, or `$HF_HOME/hub`). A file another app already downloaded isn't downloaded again, and sift-it never
  removes anything from the cache.

## Search

Run `sift-it` without arguments to search as you type, beside a preview of the selected result. Ghostty, kitty,
iTerm2 and WezTerm show pictures, PDF pages and video frames inline; audio shows a waveform with the matching time
highlighted.

| Key | Action |
|---|---|
| Enter | Open the result where it matched: its page, time, or line |
| Ctrl+R | Show it in Finder |
| Ctrl+Y | Copy its path |
| Tab | Filter by file type |
| Esc | Exit |

From scripts:

```sh
sift-it search "the diagram of the heating system"
sift-it open 1
sift-it search "annual maintenance" --kind document -n 10
sift-it search '"SERVICE-2026-042" maintenance'
sift-it search "INV-2024-0113" --by words
sift-it search "the railway bridge at sunset" --json
sift-it search "annual maintenance" | while IFS= read -r result_path; do open "$result_path"; done
```

A search is by meaning unless it says `--by words` (`search.mode` sets the default). Words still count in a search by
meaning, so names and numbers are found. Quoted phrases must match together and in order, and matching ignores case
and accents. Piped searches print one path per line; `--json` adds scores and places. `sift-it open <n>` opens a
result of the last search: a PDF in Preview at its page, audio or video in QuickTime at its time, text and code in an
editor at its line, and a conversation resumed in its folder.

Code is left out until you turn it on. Then `code:` searches and the code filter find it:

```sh
sift-it config sources.code true
sift-it index
sift-it search "code: validate a configuration file"
```

`NO_COLOR` turns off colour, and `SIFT_IT_IMAGES=0` inline pictures.

## Agents

`sift-it mcp` is an MCP server over stdio with two read-only tools: `search`, for files and past Claude Code and Codex
conversations, and `read`, for a result's text with the passages around it. It asks the engine, so the model loads
once, there. Add it to Claude Code:

```sh
claude mcp add sift-it -- sift-it mcp
```

Claude Desktop, in `~/Library/Application Support/Claude/claude_desktop_config.json`, with the full path, since apps
don't get your shell's `PATH` (`which sift-it` says it):

```json
{"mcpServers": {"sift-it": {"command": "/opt/homebrew/bin/sift-it", "args": ["mcp"]}}}
```

Codex, in `~/.codex/config.toml`:

```toml
[mcp_servers.sift-it]
command = "sift-it"
args = ["mcp"]
```

Agents with a shell can use the command itself. [skills/sift-it](skills/sift-it/SKILL.md) is an Agent Skill that
teaches them `sift-it search --json`. Put it in `~/.claude/skills` for Claude Code, or `~/.agents/skills` for Codex:

```sh
mkdir -p ~/.claude/skills/sift-it
curl -fsSL https://raw.githubusercontent.com/jwa91/sift-it/main/skills/sift-it/SKILL.md -o ~/.claude/skills/sift-it/SKILL.md
```

To search past conversations, add their folders: `sift-it folders add ~/.claude/projects ~/.codex/sessions`.

## Supported files

sift-it reads only the folders you choose, and never changes, moves or uploads a file.

| Files | Found by |
|---|---|
| Documents | the text of txt, md, rtf, doc, docx, odt, html and webarchive files, Markdown cut at its headings |
| PDFs | each page, scanned pages read by text recognition |
| Pictures | what they show, in any format macOS reads, HEIC and RAW included |
| Screenshots | what they show, and the text on them |
| Audio | what it sounds like, in 30-second windows, and what's said in it, at its time |
| Videos | their scenes, their sound, and what's said in them |
| Tags | the title, artist and album of audio and video files |
| Records | the rows of csv and tsv files, the records of json and jsonl files, the keys of yaml and toml files |
| Code and notebooks | definitions and cells, once code is turned on |
| Conversations | what you and Claude Code or Codex said in a session, which a result resumes |

Text that's mostly numbers is found by its words only, and silence isn't indexed. The model can miss or confuse sounds
and pictures: [docs/ranking.md](docs/ranking.md#limits) says where.

Conversations come from the session logs Claude Code keeps in `~/.claude/projects` and Codex in `~/.codex/sessions`.
Add those folders, and a session is found by what you and the agent said in it; tool calls, their output and the
agent's thinking aren't read. `sift-it open` resumes the session in its folder. The Mac app offers both folders with a
click.

```sh
sift-it folders add ~/.claude/projects ~/.codex/sessions
```

Files kept only in iCloud are found by name and read once they're downloaded, and stay found when macOS removes the
download; sift-it never downloads them.

sift-it skips hidden folders, `~/Library` folders inside the ones you chose, app bundles and packages, dependency and
build folders, and recognized secret files. Inside a code project it reads pictures, PDFs and audio, and the rest once
code is turned on. There are no size limits: a four-hour podcast is read as fully as a short memo, and
`sift-it status` names the files that take longest, should you want to leave them out.

## Manage the index

```sh
sift-it folders add ~/Documents
sift-it folders exclude ~/Documents/Private
sift-it estimate ~/Documents
sift-it status
sift-it service status
sift-it index
sift-it model status
sift-it selftest
```

`sift-it estimate` says what each round will have to do in a folder and about how long it takes, before it's indexed.
`sift-it --help` lists every command.

The data lives in `~/Library/Application Support/Sift It`, or the folder `SIFT_IT_HOME` names: `config.json`, the
index (`index.sqlite` and `vectors/`), `status.json`, the logs, previews, and the engine's socket, `engine.sock`.
`brew uninstall --zap sift-it` removes the app and its data, and leaves the models in the Hugging Face cache for the
other apps that may use them.

### Configuration

`config.json` holds every setting, with its value written out. `sift-it config` shows it, and
`sift-it config <key> <value>` sets one by its dotted name; `null` puts it back to its default. When the file can't be
read, the error names the key and what it takes, and the indexer waits until it's set right.

```sh
sift-it config reading.rules '[{"path": "~/Movies/Footage/**", "depth": "words"}]'
sift-it config sources.ignore '["*.psd", "~/Documents/Old"]'
sift-it config indexing.on_battery none
```

| Section | Holds | A change counts |
|---|---|---|
| `sources` | the folders, ignore patterns, the file types of each kind, whether code is read | at the next sync |
| `reading` | how far and how files are read, per kind, file type and folder | the files it affects are read again |
| `ranking` | the constants search ranks by | at the next search |
| `search` | by meaning or by words, which form of the vectors a search scans, and the memory it may take | at the next search |
| `indexing` | the memory budget, battery, and timing of the background indexer | at once |
| `log` | the size of the indexer's log | at once |

`reading.depth` sets how far a file goes: `words` (Exact), `meaning` (Embeddings), or `speech` (Transcribed, the
default). A reading value can be set for everything, per kind (`reading.video`), per file type (`reading.types.mov`),
and per folder (`reading.rules`), and the most specific wins. Ignore patterns and rule paths match a name anywhere
(`*.psd`), or a path when they hold a slash (`~/Documents/Old`, `**/Exports/**`).

`search.vectors` sets which form of the vectors a search scans. Each is always written, so a change counts at the next
search:

- `exact-f32`: every vector as it was made, 3 KB each.
- `exact-f16`: half as much to read, with scores within a thousandth.
- `sketch`: a 96-byte sketch of every vector, then exact scores for the closest.
- `auto`, the default: the most precise that fits in `search.memory_mb`, a tenth of the Mac's memory unless you set
  it. 800 MB holds exact scores for some 270,000 vectors.

## Build from source

You need Xcode 26 (for Swift 6.2), Rust, and CMake.

```sh
brew install cmake
git clone https://github.com/jwa91/sift-it
cd sift-it
make build
make check
make install
```

`make build` builds the pinned llama.cpp release, the speech helper and sift-it; `make check` runs the format checks,
Clippy and the tests; `make install` puts `sift-it` and `sift-it-transcribe` in `~/.local/bin`. With the models
downloaded, `make selftest` checks the vectors against the reference ones in `tests/reference`, and `make eval` measures
search quality on a small testbed.

- [docs/design.md](docs/design.md): what sift-it does and why, with measurements.
- [docs/architecture.md](docs/architecture.md): how the code is structured.
- [docs/protocol.md](docs/protocol.md): what front ends say to the engine.
- [docs/ranking.md](docs/ranking.md): how results are ranked, and how well.
- [docs/audio-decoding.md](docs/audio-decoding.md): how sound is decoded the same every time.

## Acknowledgements

Sift It started from [DigUp](https://github.com/ARahim3/DigUp), by Abdur Rahim: the original design and the reference
test data come from it.

## Credits

- [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), by Google DeepMind, finds files by meaning, under
  the Apache License 2.0, in [ggml-org's GGUF conversion](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF).
- [llama.cpp](https://github.com/ggml-org/llama.cpp), by the ggml authors, runs it, under the MIT License.
- [Parakeet Ultra](https://huggingface.co/moondream/parakeet-ultra), by Moondream, post-trained from NVIDIA's Parakeet
  TDT 0.6B v3, writes down speech, under CC BY 4.0, in
  [FluidInference's Core ML conversion](https://huggingface.co/FluidInference/parakeet-ultra-coreml).
- [FluidAudio](https://github.com/FluidInference/FluidAudio), by FluidInference, runs it on the Neural Engine, under the
  Apache License 2.0.

Each release carries THIRD_PARTY_NOTICES, with the license of everything built into `sift-it` and
`sift-it-transcribe`: llama.cpp, FluidAudio and the Rust crates (`scripts/notices.py` writes it).
Gemma is a trademark of Google LLC.

## License

[MIT](LICENSE).
