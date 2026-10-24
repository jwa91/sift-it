#!/usr/bin/env bash
# Builds opbench against a llama.cpp build from scripts/build-llama.sh: build.sh <vendor/llama-build…> <out>
set -euo pipefail
build=$1 out=$2
here="$(cd "$(dirname "$0")" && pwd)"
lib="$build/lib"
clang++ -std=c++17 -O2 -I"$build/include" "$here/opbench.cpp" -o "$out" \
  "$lib/libggml.a" "$lib/libggml-metal.a" "$lib/libggml-cpu.a" "$lib/libggml-blas.a" "$lib/libggml-base.a" \
  -framework Metal -framework MetalKit -framework Foundation -framework Accelerate
