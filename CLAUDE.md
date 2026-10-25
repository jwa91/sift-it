# sift-it

Search the files on a Mac by what's in them: a Rust engine and CLI around EmbeddingGemma 2 (llama.cpp with Metal,
statically linked), Parakeet Ultra (FluidAudio, in the Swift speech helper), Apple's frameworks for reading files, and
SQLite. Front ends (the Sift It app in sift-it-mac, Raycast) are clients of the engine, through `engine.sock` or
`sift-it serve` and the protocol (docs/protocol.md), or `--json`.

## CLI Family Conventions

This repo is part of the jwa91 CLI tool family (agentskills, jwa-tobrew, jwa-harden, prehandover, homebrew-tap), and
follows its conventions except where noted under "Exceptions".

### Import Contract (ADR 0009)
- Process-level integration only — no source imports between sibling CLIs
- homebrew-tap is a pure distribution sink

### Repo Structure
- `Makefile` (with `build`/`install`/`check`/`release` targets), codesign script, CI + release workflows
- One binary per repo, name matches repo name
- Archive naming: `{name}_{version}_{os}_{arch}.tar.gz`, with `checksums.txt`

### Distribution (ADR 0008)
- `brew install jwa91/tap/sift-it` installs Sift It.app, which carries this repo's release in `Contents/Helpers`, and
  links the `sift-it` command from there. That cask is released from sift-it-mac (see Exceptions): a release here
  publishes the archive, and reaches people when sift-it-mac's `ENGINE_VERSION` names it.

### Release Process
- Locally: `make release VERSION=x.y.z` runs the preflight checks (VERSION set and equal to Cargo.toml's, gh auth'd,
  this Mac can sign and notarize (`scripts/codesign.sh --check`), tag at HEAD), then `scripts/release.sh x.y.z`.
  What a Mac needs for it: the header of `scripts/codesign.sh`.
- `make snapshot`: everything a release makes, in `dist/`, without publishing
- `workflow_dispatch` release workflow: verify → snapshot → smoke (it builds and checks; publishing stays local until
  CI has signing creds)

### Secrets (ADR 0005)
- No `.env` files ever. sift-it has no `.env.template` either (see Exceptions): nothing in it resolves secrets

## Exceptions (Rust, macOS-only)

These family rules are Go-specific and don't apply here:

- **Rust, not Go.** There is no `go.mod`, `CGO_ENABLED`, golangci-lint or govulncheck. `make check` is
  `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`, after building the speech helper.
- **No GoReleaser.** Its Rust builder always runs `rustup target add`, which a toolchain not managed by rustup doesn't
  allow, and its prebuilt builder is GoReleaser Pro. `scripts/release.sh` does the same steps: build, codesign +
  notarize, `sift-it_{version}_darwin_arm64.tar.gz` + `checksums.txt`, and a GitHub release with the gh login.
- **The cask comes from the app.** `Casks/sift-it.rb` is written by sift-it-mac's release: one cask with the app and the
  command is one thing for Homebrew to trust, where a cask depending on another in an untrusted tap fails to install.
- **darwin/arm64 only**, not `darwin+linux × amd64+arm64`. sift-it needs Metal (llama.cpp) and Apple's frameworks
  (PDFKit, ImageIO, Vision, AVFoundation, and Core ML for speech). The cask says `depends_on macos: :sonoma` (and later)
  and `depends_on arch: :arm64`.
- **A native build step.** `scripts/build-llama.sh` builds llama.cpp at a pinned release before cargo (a `make`
  prerequisite), and `make helper` builds the speech helper with SwiftPM (Swift 6.2, so Xcode 26). CI runs on
  `macos-15` with Xcode 26 selected, not `ubuntu-latest`.
- **No `.env.template`, no 1Password references.** The release needs no secrets of its own: the Developer ID
  certificate and a notarytool profile live in the releasing Mac's keychain (set up once per Mac; see
  `scripts/codesign.sh`), and the GitHub release uses the gh login. The repo holds no credentials
  and no references to any.
- **Two binaries.** `sift-it-transcribe` (transcribe/, the speech helper) ships beside `sift-it`: it's Swift, because
  FluidAudio, which runs the speech model on the Neural Engine through Core ML, is a Swift package. A process of its
  own also gives all of its memory back, the Neural Engine's included, when there's nothing to write down. The release
  signs and notarizes both, and Sift It.app carries both.
- **No separate notarize script.** `scripts/codesign.sh` signs both binaries and notarizes them in one submission,
  after the build.

## Working on it

- `docs/design.md` says what sift-it does and what's planned for later; `docs/architecture.md` says where code goes
  and which way dependencies point. New code follows its rules.
- `make check` before every commit. `make selftest` after anything that could change vectors (llama.cpp, the model
  files, image sizing, prompts): every vector must keep cosine ≥ 0.999 with tests/reference.
- Nothing adds to the environment once other threads run: a process started meanwhile can fail with "Bad address".
  The variables llama.cpp sets are set first thing in `main` (`embedder::set_environment`), and `make selftest` checks
  that loading the models changes nothing more.
- `make eval` (testbed + evals/evals.json) after anything that touches ranking or readers. Currently 54/55 top-1, 19
  of 20 at the right place, 0/4 false alarms. "someone who can fix a leaking tap" finds contacts.csv first, but too
  weakly to show it (a table's many rows lower its score); "penguin in the film" opens where the narration names it,
  a few seconds before it's shown.
- The index records what made its vectors (`embedder::VECTOR_SOURCE`): the model and the pipeline's version, not the
  llama.cpp release. Bump the version when vectors change, on purpose or with a llama.cpp release that fails
  `make selftest`, and indexes re-embed. When a kind is read or cut differently, bump its `ingest::read::version` instead:
  only its files are read again.
