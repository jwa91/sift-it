#!/usr/bin/env bash
# The energy suite: every workload as fixed work, one after another with cool-downs between (a fanless Mac carries heat
# from one run into the next), each logged with its wall-clock start and end so power samples taken meanwhile
# (`sudo powermetrics … -o power.txt`) can be cut per workload by power.py, or watched live with live.py. The busiest
# processes are logged every 5 s beside it, so a run that overlapped other work shows.
#
#   suite.sh <scratch> [full|short|quick]
#
# full (about 40 min): all 2,856 passages, 62 min of speech, 100 pictures and sounds, both orders, and four minutes
# sustained of each text encoder. short (about 11 min): 1,000 passages, the 36-minute English podcast, 30 pictures
# and sounds, both orders. quick: the short suite without cool-downs, to check the script.
#
# Needs in the environment: SIFT_IT_MODELS (the GGUF snapshot), COREML_TEXT (embeddinggemma-2-coreml snapshot),
# COREML_CACHE, B11461 and B11856 (sift-it binaries), TRANSCRIBE, GGUF, FLUID, COREML (speech.py's), PASSAGES (texts
# from SIFT_IT_DUMP_TEXTS) and SPEECH (the WAVs, space-separated). POWER, if set, names the file powermetrics writes:
# the suite starts at its first sample.
set -uo pipefail
scratch=$1
mode=${2:-full}
here="$(cd "$(dirname "$0")" && pwd)"
results="$scratch/results"
log="$results/suite.jsonl"
mkdir -p "$results" "$scratch/homes/suite"
export SIFT_IT_HOME="$scratch/homes/suite"
case $mode in
  full) cool=60 media=100 idle_s=60 ;;
  short | quick)
    cool=20 media=30 idle_s=30
    [[ $mode == quick ]] && cool=2
    head -1000 "$PASSAGES" > "$results/passages-1000.jsonl"
    PASSAGES="$results/passages-1000.jsonl"
    SPEECH=${SPEECH%% *}
    ;;
  *) echo "usage: suite.sh <scratch> [full|short|quick]" >&2; exit 2 ;;
esac

now() { python3 -c 'import time; print(time.time())'; }
mark() { printf '{"name":"%s","wall_start":%s,"wall_end":%s,"output":%s}\n' "$1" "$2" "$3" \
  "$(python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' < "$4")" >> "$log"; }
step() {
  local name=$1
  shift
  local out="$results/suite-$name.out" start end
  echo "$(date +%T) $name" >&2
  start=$(now)
  "$@" > "$out" 2>&1
  end=$(now)
  mark "$name" "$start" "$end" "$out"
  echo "$(date +%T) cool" >&2
  sleep "$cool"
}
idle() { sleep "$1"; }
texts() { "$1" bench-texts "$PASSAGES" "${@:2}"; }
# Core ML with the two shapes worth their memory (each shape holds its own ~300 MB copy of the weights on the ANE).
coreml() { SIFT_IT_COREML_FUNCTIONS=256,512 SIFT_IT_COREML="$COREML_TEXT" SIFT_IT_COREML_CACHE="$COREML_CACHE" "$@"; }
speech() { uv run -q --with numpy python -I "$here/speech.py" "$1" "$2" "$results/suite-$3.jsonl" $SPEECH; }

( while sleep 5; do echo "$(date +%s) $(ps -Ao pcpu,comm -r | sed -n 2,4p | awk '{printf "%s %s; ", $1, $NF}')"; done ) \
  > "$results/suite-load.txt" &
watcher=$!
trap 'kill $watcher' EXIT

: > "$log"
if [[ -n "${POWER:-}" ]]; then
  echo "$(date +%T) waiting for $POWER" >&2
  until [[ -s "$POWER" ]] && grep -q "Sampled system activity" "$POWER" 2>/dev/null; do sleep 2; done
fi
pmset -g batt > "$results/battery-start.txt"
step idle-start idle "$idle_s"
step text-llama-b11461 texts "$B11461"
step text-coreml-ane coreml texts "$B11461" --text-only
step text-llama-b11856 texts "$B11856"
if [[ $mode == full ]]; then
  # The same Core ML model on the GPU: is it the Neural Engine or Core ML? (on a quarter of the texts)
  head -700 "$PASSAGES" > "$results/passages-700.jsonl"
  step text-coreml-gpu env SIFT_IT_COREML_UNITS=gpu SIFT_IT_COREML_FUNCTIONS=256,512 SIFT_IT_COREML="$COREML_TEXT" \
    SIFT_IT_COREML_CACHE="$COREML_CACHE" "$B11461" bench-texts "$results/passages-700.jsonl" --text-only
fi
step media-b11461 "$B11461" bench-media "$media"
step media-b11856 "$B11856" bench-media "$media"
step speech-transcribe-cpp speech "$TRANSCRIBE" "$GGUF" transcribe-cpp
step speech-fluid-ane speech "$FLUID" "$COREML" fluid-ane
# The same again in the other order, so warming up over the suite doesn't favour either side.
step speech-fluid-ane-2 speech "$FLUID" "$COREML" fluid-ane-2
step speech-transcribe-cpp-2 speech "$TRANSCRIBE" "$GGUF" transcribe-cpp-2
step text-coreml-ane-2 coreml texts "$B11461" --text-only
step text-llama-b11461-2 texts "$B11461"
if [[ $mode == full ]]; then
  # Sustained: four minutes each, to see where a fanless Mac settles.
  step sustained-llama-b11856 texts "$B11856" --seconds 240
  step sustained-coreml-ane coreml texts "$B11461" --text-only --seconds 240
fi
step idle-end idle "$idle_s"
pmset -g batt > "$results/battery-end.txt"
echo "$(date +%T) done" >&2
