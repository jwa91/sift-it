---
name: sift-it
description: Search the files on this Mac by what's in them (documents, notes, PDFs, pictures, audio, video) and past Claude Code and Codex conversations, with the `sift-it` command. Use when the user asks to find a file, a photo or a recording by what it says or shows, wants context from their own documents, or refers to an earlier conversation ("the conversation where we…", "what did we decide about…").
compatibility: macOS with Sift It installed (brew install jwa91/tap/sift-it), so `sift-it` is on the PATH.
---

# Sift It

`sift-it` searches an index of the folders the user chose, by meaning and by words. It's fast: search freely.

## Search

```sh
sift-it search "the warranty for the washing machine" --json -n 5
sift-it search '"INV-2024-0113" invoice' --json
sift-it search "what we decided about the release" --kind conversation --json
```

- Say what you're looking for in plain words: meaning finds paraphrases and other languages.
- Put exact words, names, numbers and codes in double quotes inside the query: a quoted phrase must be in a result,
  its words together and in order. `--by words` searches by words alone.
- `--kind` keeps to some kinds (comma-separated): `plaintext`, `document`, `structured`, `image`, `screenshot`,
  `audio`, `video`, `conversation`, `code`. Code is only searched with `--kind code`, and only if the user turned it
  on.
- `-n` is how many results (10 by default).

`--json` prints `{"hits": [...], "meaning": true}`, best first. In each hit:

- `path`, `kind`, and `name`: the file's name, or a conversation's title;
- `loc`, `loc_end`: where it matched, by `segment`: a line (`passage`), a page (`page`), seconds (`speech`, `sound`,
  `frame`), or for a conversation (`exchange`) when it was said, in seconds since 1970;
- `excerpt`: about 300 characters around the match;
- `strong`: a clear match (the rest are weaker); `score` ranks them;
- `info`: what the reader found (pages, duration, tags), and a conversation's details.

`"meaning": false` means the model wasn't available, and it searched by words.

## Use what's found

- Read the file at `path` yourself for more: a text file at line `loc`, a PDF at page `loc`.
- Don't run `sift-it open` unless the user asks to see a result: it opens it in an app on their screen.

## Past conversations

- With `--kind conversation`, or among other results, come Claude Code's and Codex's session logs (if the user added
  `~/.claude/projects` or `~/.codex/sessions`).
- The `excerpt` is the matching exchange, as `User: …` and `Claude: …` (or `Codex: …`).
- `info.resume` is the command that resumes the session, to run in `info.cwd`. Give the user
  `cd <cwd> && claude --resume <id>` rather than running it.
- `path` is the raw log (JSON lines, mostly tool output): don't read it whole. For more of a conversation, use the
  Sift It MCP server's `read` tool if it's connected, or search again with words from the exchange.

## When nothing is found

`sift-it status` says what's indexed and how far indexing is. Only the user's chosen folders are indexed
(`sift-it folders`). Don't change folders or settings unless the user asks.
