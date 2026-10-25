# Protocol

What sift-it's clients and its engine say to each other. The CLI, the TUI and `sift-it serve` speak it, and so does
any front end. Its types are in `crates/protocol` (`sift-it-protocol`); this describes the wire for clients in other
languages.

## Getting there

- **The engine's socket:** a Unix socket, `engine.sock` in the data folder (`~/Library/Application Support/Sift It`,
  or `SIFT_IT_HOME`). Only this user can open it. When the data folder is too deep for a socket's name, the socket is
  reached through a link in the user's temporary folder (services/engine.rs says how).
- **`sift-it serve`:** the same lines over its stdin and stdout. It starts the engine when it isn't running, starts it
  again when it stops, and answers two requests itself: `preview` and `quit`.

The engine starts at login (its LaunchAgent), or with the first client.

## Lines

JSON lines, one message per line, in UTF-8.

- **A request:** `{"id": …, "op": …, …}`. The `id` is any JSON value, and comes back in the reply. The rest depends on
  the op.
- **A reply:** the op's reply with the request's `id`, or a failure: `{"id": …, "ok": false, "error": "…"}`. Replies
  can come in another order than their requests (a preview takes longer than a search).
- **An event:** a line with `event` and no `id`, to every client.

The first line a client gets is the `ready` event, with the engine's build. A client of another build stops that
engine (`stop`) and starts its own; there's no negotiating, a mismatch is only noticed.

## Requests

### search

```json
{"id": 1, "op": "search", "q": "pancake recipe", "mode": "meaning", "kinds": ["document"], "limit": 40,
 "prefix_last": true, "wait": false}
```

Only `q` is needed. `mode` is `words` or `meaning` (config.json's `search.mode` when left out). `kinds` filters by
kind (all but code when empty; `code` alone, or a `code:` prefix in `q`, searches code). `limit` defaults to 40.
`prefix_last` matches the last word as a prefix, while someone types it. While the model loads, a search by meaning is
answered by words (`"meaning": false` in the reply), unless `wait` is true. An empty `q` lists recent files.

Reply:

```json
{"id": 1, "hits": [Hit, …], "meaning": true, "ms": 3.1}
```

### recent

```json
{"id": 2, "op": "recent", "kinds": ["image"], "limit": 40}
```

Recently changed files, newest first. The reply is a search's.

### text

```json
{"id": 3, "op": "text", "segment_id": 12}
{"id": 3, "op": "text", "path": "/Users/me/notes.md"}
```

A segment's whole text, or a file's (its text segments in order). Reply: `{"id": 3, "text": "…"}` (`null` when there's
none).

### status

```json
{"id": 4, "op": "status"}
```

Reply:

```json
{"id": 4, "meaning": "ready", "error": null, "vectors": 6500, "scan": "exact-f32", "resident": 21000000,
 "stats": Stats, "indexer": IndexerStatus}
```

`meaning` is `unloaded`, `loading`, `ready` or `unavailable` (then `error` says why). `vectors` is how many are
mapped, `scan` the form searches scan (what `auto` picked), `resident` how many bytes of them are in memory.
`indexer` is the background indexer's progress as it last said it, `null` when it never ran for this data folder.

### config

```json
{"id": 5, "op": "config"}
{"id": 5, "op": "config", "key": "search.mode"}
{"id": 5, "op": "config", "key": "search.mode", "value": "words"}
{"id": 5, "op": "config", "key": "sources.ignore", "value": "[\"*.psd\"]"}
```

The settings, or the one at a dotted key, set first when there's a `value`: what `sift-it config <key> <value>` does.
`value` is always a string, read as JSON, or else taken as the string it is: `"4096"` is a number, `"words"` a
string, and `"null"` puts the setting back to its default. Reply:
`{"id": 5, "config": …}`. The indexer follows the change at once, and the engine at its next request.

### folders

```json
{"id": 6, "op": "folders"}
{"id": 6, "op": "folders", "action": "add", "paths": ["~/Documents"]}
```

The folders indexed, changed first when there's an `action`: `add`, `remove` (and forget what was left out of it),
`exclude` (leave a folder inside an indexed one out) or `include` (index it again). Reply:

```json
{"id": 6, "folders": ["/Users/me/Documents"], "excluded": ["/Users/me/Documents/Old"]}
```

### unload, stop

```json
{"id": 7, "op": "unload"}
{"id": 8, "op": "stop"}
```

`unload` lets the model and the vectors go until a search by meaning needs them; `stop` ends the engine. Reply:
`{"id": 7, "ok": true}`.

### preview (sift-it serve)

```json
{"id": 9, "op": "preview", "path": "/Users/me/scan.pdf", "kind": "pdf", "loc": 3, "size": 512}
```

A picture to show beside a result: `kind` is `image`, `pdf` (page `loc`), `video` (the frame at `loc` seconds) or
`waveform` (an audio file's, with `loc` to `loc_end` seconds highlighted). `size` is the longest side in pixels (640
when left out). Reply: `{"id": 9, "image": "/…/preview.jpg"}`.

### quit (sift-it serve)

```json
{"id": 10, "op": "quit"}
```

Ends `sift-it serve` once the requests before it are answered: `{"id": 10, "ok": true}`. The engine goes on.

## Events

```json
{"event": "ready", "version": "0.4.1", "build": "0.4.1+3f2a…", "index": "/…/index.sqlite", "meaning": "unloaded"}
{"event": "meaning", "state": "loading"}
{"event": "meaning", "state": "unavailable", "error": "embeddinggemma-2-Q8_0.gguf isn't downloaded (run `sift-it model download`)"}
{"event": "config", "error": "can't read config.json: …"}
{"event": "indexer", "state": "indexing", "rounds": [{"round": "words", "state": "done", …}, …], …}
```

- `ready`: the first line on connecting.
- `meaning`: search by meaning loads, is ready, can't be had, or was let go.
- `config`: config.json can't be read; the settings stay as they were.
- `indexer`: the background indexer's progress changed (IndexerStatus, below).

## Types

**Hit:** a file found, and where in it.

| Field | |
|---|---|
| `path`, `name` | the file, and what it's called: its name, or a conversation's title |
| `kind` | its Kind |
| `segment`, `segment_id` | the segment that matched best, where the result opens |
| `loc`, `loc_end` | where in the file: a line, a page or a second, by the segment; for an exchange, when it was said (seconds since 1970) |
| `score` | what it's ranked by |
| `lift`, `cosine` | how far its meaning stands above the query's median, and its cosine (`null` by words) |
| `meaning`, `words`, `strong` | it matched by meaning; all of the query's words are in it; it's a strong match |
| `excerpt` | about 300 characters around the query's words |
| `modified`, `size` | seconds since 1970, bytes |
| `info` | what the reader found out: pages, duration, size in pixels, tags; for a conversation, see below |

**Kind:** `plaintext`, `document`, `structured` (csv, tsv, json, jsonl, yaml, toml), `image`, `screenshot`, `audio`,
`video`, `code`, `conversation` (a Claude Code or Codex session log, known by its first lines).

**Segment:** `passage`, `page`, `picture`, `ocr`, `sound`, `frame`, `speech`, `exchange` (a prompt and what the agent
answered, or a part of a long one).

**A conversation's `info`:** `tags.title` (its title: the one the app or the agent gave it, else its first prompt),
`agent` (`claude` or `codex`), `session` (its id), `cwd` (the folder it ran in) and `resume`, the command that resumes
it, as words, to run in `cwd`: `["claude", "--resume", "<session>"]` or `["codex", "resume", "<session>"]`. A
segment's text reads as the conversation: `User: …`, then `Claude: …` or `Codex: …`, where the speaker changes.

**Stats:** `files`, `done`, `pending`, `failed`, `cloud` (files only in iCloud whose work waits for their download:
found by their names, or by what was read of them before macOS removed their download), `segments`, `vectors`,
`by_kind` (files by kind), `vectors_by_kind` (vectors by their file's kind, keyed like `by_kind`; a kind with none
is left out), `bytes` (the index on disk).

**IndexerStatus:** `pid`, `state`, `detail` (what the work that's left waits for: a charger, memory, a model, or a
config.json that can be read, with the key that keeps it from it),
`rounds` (a RoundStatus for each round this index has had work in, in their order), `current` (the file it did last),
`updated` (seconds since 1970) and `last_error`. `state` is `starting`, `syncing`, `indexing` (its rounds say how
far), `waiting` (nothing can run: `detail` says why), `paused`, `idle` or `stopped`. The indexer also writes it to
`status.json` in the data folder.

**RoundStatus:** how far a round of indexing is (docs/design.md, "Rounds").

| Field | |
|---|---|
| `round` | `words`, `text`, `ocr`, `pictures`, `sound` or `speech`: the order they come in |
| `state` | `done`, `running`, `queued` (it waits for its turn: another round has its chip, or the memory budget has no room for its tool yet) or `waiting` (for what the indexer's `detail` says) |
| `done`, `total` | in the round's unit: files (words), passages (text), pictures (OCR and pictures; a video counts a frame every 2 s), seconds of sound (sound and speech). The total grows while the rounds before it find more: the text round's passages, as words, OCR and speech read them |
| `files` | the files with something left to do in it |
| `seconds_left` | about how long the rest takes, at the speed it went while it ran; `null` until it has run a while |

So "words: done, text: 60%, pictures: 10%, speech: waiting" is `done / total` of each, with its `state`.

**Phases.** Front ends show the rounds as three phases, named the same everywhere:

| Phase | Rounds | Depth (`reading.depth`) |
|---|---|---|
| Exact | `words`, `ocr` | `words` |
| Embeddings | `text`, `pictures`, `sound` | `meaning` |
| Transcribed | `speech`, and `sound` when the index has a `speech` round | `speech` |

A file's sound round waits for its speech, so in an index that writes down what's said, `sound` finishes with
Transcribed. A phase's progress is the mean of its rounds' `done / total`, since their units differ, and its time left
the longest of their `seconds_left`, since they run side by side.
