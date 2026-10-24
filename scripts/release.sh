#!/usr/bin/env bash
# Releases sift-it: builds it, signs and notarizes it, and publishes the archive as a GitHub release. That's what
# GoReleaser does for the rest of the jwa91 family; sift-it does it here because GoReleaser's Rust builder always runs
# `rustup target add`, which a toolchain not managed by rustup doesn't allow (see CLAUDE.md, Exceptions). Same archive
# name and checksums file as GoReleaser's.
#
# The `sift-it` cask isn't written here: sift-it-mac releases it, with Sift It.app carrying this archive's binaries (its
# ENGINE_VERSION names the release) and the cask linking the `sift-it` command from inside the app.
#
#   scripts/release.sh X.Y.Z        a release (through `make release`)
#   scripts/release.sh --snapshot   build and archive into dist/, unsigned; publish nothing
#
# It needs no secrets of its own: signing and notarizing use the releasing Mac's keychain (scripts/codesign.sh), and the
# GitHub release uses your gh login.
set -euo pipefail

OWNER=jwa91
REPO=sift-it

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

snapshot=false
if [[ "${1:-}" == "--snapshot" ]]; then
  snapshot=true
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
else
  version="${1:?usage: scripts/release.sh X.Y.Z | --snapshot}"
fi
tag="v$version"
archive="sift-it_${version}_darwin_arm64.tar.gz"
stage="dist/sift-it_darwin_arm64"

rm -rf dist
mkdir -p "$stage"

# Build, and check it is the version being released.
make build
built="$(target/release/sift-it --version | awk '{print $2}')"
if [[ "$built" != "$version" ]]; then
  echo "release: the build says $built, not $version (Cargo.toml)" >&2
  exit 1
fi

# Sign and notarize: a release always is; a snapshot (CI's, or a dry run) isn't. sift-it-transcribe, the speech helper,
# goes beside sift-it: it's Swift (FluidAudio, on the Neural Engine), built by `make build` too.
cp target/release/sift-it target/release/sift-it-transcribe "$stage/"
if $snapshot; then
  echo "snapshot: not signed"
else
  scripts/codesign.sh "$stage/sift-it" "$stage/sift-it-transcribe"
fi

# The archive (the binaries with the license and docs, as GoReleaser packs them) and its checksum.
cp LICENSE README.md CHANGELOG.md "$stage/"
tar -czf "dist/$archive" -C "$stage" sift-it sift-it-transcribe LICENSE README.md CHANGELOG.md
(cd dist && shasum -a 256 "$archive" > checksums.txt)
sha256="$(cut -d' ' -f1 dist/checksums.txt)"

# The release notes: this version's section of the changelog.
awk -v version="$version" '
  $0 ~ "^## \\[" version "\\]" { inside = 1; next }
  /^## \[/ { inside = 0 }
  inside { print }
' CHANGELOG.md > dist/notes.md
if [[ ! -s dist/notes.md ]]; then
  echo "release: CHANGELOG.md has no section for [$version]" >&2
  $snapshot || exit 1
fi

if $snapshot; then
  echo "snapshot in dist/: $archive ($sha256)"
  exit 0
fi

# The GitHub release, with your gh login.
gh release create "$tag" "dist/$archive" dist/checksums.txt \
  --repo "$OWNER/$REPO" --title "$tag" --notes-file dist/notes.md --verify-tag

echo "released $tag: set ENGINE_VERSION in sift-it-mac to $version and release it, for the cask"
