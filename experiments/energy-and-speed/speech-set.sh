#!/usr/bin/env bash
# Times transcribe.cpp and FluidAudio (ANE, then GPU) on the same sound: speech-set.sh <scratch> <tag> <wav>...
# Needs: $TRANSCRIBE (sift-it-transcribe), $GGUF (parakeet-ultra-Q8_0.gguf), $FLUID (fluid-transcribe) and
# $COREML (the parakeet-ultra-coreml folder).
set -euo pipefail
scratch=$1 tag=$2
shift 2
here="$(cd "$(dirname "$0")" && pwd)"
run() { uv run -q --with numpy python -I "$here/speech.py" "$@"; }
mkdir -p "$scratch/results"
run "$TRANSCRIBE" "$GGUF" "$scratch/results/tcpp-$tag.jsonl" "$@"
run "$FLUID" "$COREML" "$scratch/results/fluid-ane-$tag.jsonl" "$@"
FLUID_UNITS=gpu run "$FLUID" "$COREML" "$scratch/results/fluid-gpu-$tag.jsonl" "$@"
