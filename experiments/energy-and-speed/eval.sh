#!/usr/bin/env bash
# `make eval` for one variant of the engine, in an index of its own:
#   eval.sh <name> <sift-it binary> <testbed> <scratch>     (extra settings come in through the environment)
# e.g. SIFT_IT_COREML=<folder> eval.sh coreml target/release/sift-it /tmp/testbed /tmp/scratch
set -euo pipefail
name=$1 binary=$2 testbed=$3 scratch=$4
root="$(cd "$(dirname "$0")/../.." && pwd)"
export SIFT_IT_HOME="$scratch/homes/eval-$name"
rm -rf "$SIFT_IT_HOME"
"$binary" folders add "$testbed" >/dev/null
"$binary" config sources.code true >/dev/null
start=$(date +%s)
"$binary" index >/dev/null 2>&1
echo "$name: indexed in $(( $(date +%s) - start )) s"
"$binary" eval "$root/evals/evals.json"
