# sift

Search the files on a Mac by what's in them: a Rust CLI around EmbeddingGemma 2 (llama.cpp with Metal, statically
linked), Apple's frameworks for reading files, and SQLite. The Raycast extension (a separate repo) is a thin front end
over `sift serve`.

## CLI Family Conventions

This repo is part of the jwa91 CLI tool family (agentskills, jwa-tobrew, jwa-harden, prehandover, homebrew-tap), and
follows its conventions except where noted under "Exceptions".

### Import Contract (ADR 0009)
- Process-level integration only — no source imports between sibling CLIs
- homebrew-tap is a pure distribution sink

### Repo Structure
- `.goreleaser.yaml`, `Makefile` (with `build`/`install`/`check`/`release` targets), codesign script, CI + release
  workflows, `.env.template`
- One binary per repo, name matches repo name

### GoReleaser
- `version: 2`, post-build codesign hook, modern `homebrew_casks:` block (never deprecated `brews:`)
- Archive naming: `{name}_{version}_{os}_{arch}.tar.gz`

### Distribution (ADR 0008)
- Ships as a Homebrew Cask into `jwa91/homebrew-tap/Casks/` (`brew install jwa91/tap/sift`)

### Release Process
- Locally: `make release VERSION=x.y.z` runs the preflight checks (VERSION set and equal to Cargo.toml's, 1Password
  signed in, gh auth'd, notarytool ready, tag at HEAD), then `jwa-harden run -- goreleaser release --clean`
  (or `op run --env-file=.env.template --` where jwa-harden isn't installed)
- `workflow_dispatch` release workflow: verify → release → smoke (unsigned; manual-only until CI has signing creds)

### Secrets (ADR 0005)
- No `.env` files ever — only `.env.template` with `op://` references resolved at runtime through 1Password

## Exceptions (Rust, macOS-only)

These family rules are Go-specific and don't apply here:

- **Rust, not Go.** GoReleaser's Rust builder runs `cargo build`; there is no `go.mod`, `CGO_ENABLED`, golangci-lint or
  govulncheck. `make check` is `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`.
- **darwin/arm64 only**, not `darwin+linux × amd64+arm64`. sift needs Metal (llama.cpp) and Apple's frameworks
  (PDFKit, ImageIO, Vision, AVFoundation). The cask says `depends_on macos: ">= :sonoma"` and `depends_on arch: :arm64`.
- **A native build step.** `scripts/build-llama.sh` builds llama.cpp at a pinned release before cargo (a GoReleaser
  `before` hook, and a `make` prerequisite). CI runs on `macos-15`, not `ubuntu-latest`.
- **No separate notarize script.** `scripts/codesign.sh` signs and notarizes in one post-build hook; there is only one
  darwin binary.

## Working on it

- `make check` before every commit. `make selftest` after anything that could change vectors (llama.cpp, the model
  files, image sizing, prompts): every vector must keep cosine ≥ 0.999 with tests/reference.
- `make eval` (testbed + evals/evals.json) after anything that touches ranking or readers. Currently 43/43 top-1.
- The index records which pipeline made its vectors (`embedder::vector_source`); bump its version when vectors change
  on purpose, and indexes re-embed.
