#!/usr/bin/env bash
# Builds llama.cpp (and its multimodal library, mtmd) as static libraries with Metal, for the Rust engine to link.
#
#   scripts/build-llama.sh          → vendor/llama-build/{lib,include}
#
# The release is pinned, with sift-it's change to mtmd (scripts/llama.cpp.patch). Another release can make slightly
# different vectors, which would make an existing index disagree with new queries: bump TAG on purpose, then run
# `make selftest`. When it passes, indexes keep their vectors; when it doesn't, bump `embedder::VECTOR_SOURCE`.
set -euo pipefail

TAG=b11856
COMMIT=3642238cc12050c8ab70afdf4b3121ea365c7579

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCE="$ROOT/vendor/llama.cpp"
BUILD="$ROOT/vendor/llama.cpp/build-sift"
OUT="$ROOT/vendor/llama-build"
PATCH="$ROOT/scripts/llama.cpp.patch"

if [[ ! -d "$SOURCE/.git" ]]; then
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$SOURCE"
fi
if [[ "$(git -C "$SOURCE" rev-parse HEAD)" != "$COMMIT" ]]; then
  echo "vendor/llama.cpp isn't at $TAG ($COMMIT); delete it and run this again" >&2
  exit 1
fi

# The patch, unless it's applied already.
if ! git -C "$SOURCE" apply --reverse --check "$PATCH" 2> /dev/null; then
  git -C "$SOURCE" apply "$PATCH"
fi

cmake -S "$SOURCE" -B "$BUILD" -G "Unix Makefiles" -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 \
  -DBUILD_SHARED_LIBS=OFF -DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON -DGGML_NATIVE=OFF \
  -DLLAMA_BUILD_COMMON=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=OFF \
  -DLLAMA_BUILD_APP=OFF -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_MTMD=ON -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF > /dev/null
cmake --build "$BUILD" --config Release -j "$(sysctl -n hw.ncpu)" --target llama mtmd > /dev/null

rm -rf "$OUT"
mkdir -p "$OUT/lib" "$OUT/include"
find "$BUILD" -name '*.a' \( -name 'libllama.a' -o -name 'libmtmd.a' -o -name 'libggml*.a' -o -name 'libvendor-*.a' \) -exec cp {} "$OUT/lib/" \;
cp "$SOURCE/include/llama.h" "$SOURCE"/ggml/include/*.h "$SOURCE/tools/mtmd/mtmd.h" "$SOURCE/tools/mtmd/mtmd-helper.h" \
  "$OUT/include/"
echo "$TAG" > "$OUT/TAG"
echo "built llama.cpp $TAG into $OUT:"
ls "$OUT/lib"
