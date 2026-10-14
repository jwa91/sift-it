# sift-it

Search the files on a Mac by what's in them: a Rust CLI around EmbeddingGemma 2 (llama.cpp with Metal, statically
linked), Apple's frameworks for reading files, and SQLite. The Raycast extension (a separate repo) is a thin front end
over `sift-it serve`.

## CLI Family Conventions

This repo is part of the jwa91 CLI tool family (agentskills, jwa-tobrew, jwa-harden, prehandover, homebrew-tap), and
follows its conventions except where noted under "Exceptions".

### Import Contract (ADR 0009)
- Process-level integration only — no source imports between sibling CLIs
- homebrew-tap is a pure distribution sink

### Repo Structure
- `Makefile` (with `build`/`install`/`check`/`release` targets), codesign script, CI + release workflows,
  `.env.template`
- One binary per repo, name matches repo name
- Archive naming: `{name}_{version}_{os}_{arch}.tar.gz`, with `checksums.txt`

### Distribution (ADR 0008)
- Ships as a Homebrew Cask into `jwa91/homebrew-tap/Casks/` (`brew install jwa91/tap/sift-it`)

### Release Process
- Locally: `make release VERSION=x.y.z` runs the preflight checks (VERSION set and equal to Cargo.toml's, 1Password
  signed in, gh auth'd, notarytool ready, tag at HEAD), then `jwa-harden run -- scripts/release.sh x.y.z`
  (or `op run --env-file=.env.template --` where jwa-harden isn't installed)
- `make snapshot`: everything a release makes, in `dist/`, without publishing
- `workflow_dispatch` release workflow: verify → snapshot → smoke (it builds and checks; publishing stays local until
  CI has signing creds)

### Secrets (ADR 0005)
- No `.env` files ever — only `.env.template` with `op://` references resolved at runtime through 1Password

## Exceptions (Rust, macOS-only)

These family rules are Go-specific and don't apply here:

- **Rust, not Go.** There is no `go.mod`, `CGO_ENABLED`, golangci-lint or govulncheck. `make check` is
  `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`.
- **No GoReleaser.** Its Rust builder always runs `rustup target add`, which this machine's toolchain policy reserves for
  mise (dotfiles toolchain-shim), and its prebuilt builder is GoReleaser Pro. `scripts/release.sh` does the same
  steps: build, codesign + notarize, `sift-it_{version}_darwin_arm64.tar.gz` + `checksums.txt`, a GitHub release, and
  `Casks/sift-it.rb` committed to the tap through the API with the tap-writer token (commit message
  `feat(sift-it): cask update to vX.Y.Z`).
- **darwin/arm64 only**, not `darwin+linux × amd64+arm64`. sift-it needs Metal (llama.cpp) and Apple's frameworks
  (PDFKit, ImageIO, Vision, AVFoundation). The cask says `depends_on macos: :sonoma` (and later) and `depends_on arch: :arm64`.
- **A native build step.** `scripts/build-llama.sh` builds llama.cpp at a pinned release before cargo (a `make`
  prerequisite). CI runs on `macos-15`, not `ubuntu-latest`.
- **No separate notarize script.** `scripts/codesign.sh` signs and notarizes in one post-build hook; there is only one
  darwin binary.

## Working on it

- `make check` before every commit. `make selftest` after anything that could change vectors (llama.cpp, the model
  files, image sizing, prompts): every vector must keep cosine ≥ 0.999 with tests/reference.
- `make eval` (testbed + evals/evals.json) after anything that touches ranking or readers. Currently 43/43 top-1.
- The index records which pipeline made its vectors (`embedder::vector_source`); bump its version when vectors change
  on purpose, and indexes re-embed.
