#!/usr/bin/env bash
# Memory of a text encoder while it embeds, alone and as two processes at once (footprint de-duplicates the pages
# they share, so the pair's total says whether the weights are shared):
#   memory.sh <scratch> <label> <command…>      (the command must keep embedding for ~40 s: bench-texts --seconds 40)
set -euo pipefail
scratch=$1 label=$2
shift 2
out="$scratch/results/memory-$label.txt"
mkdir -p "$scratch/results"
"$@" > /dev/null 2>&1 &
a=$!
sleep 20
{ echo "== $label, one process"; footprint -p $a 2>/dev/null | sed -n '1,25p'; } > "$out"
"$@" > /dev/null 2>&1 &
b=$!
sleep 20
{ echo "== $label, two processes"; footprint -p $a -p $b 2>/dev/null | sed -n '1,40p'; echo "== vmmap $a"; vmmap --summary $a 2>/dev/null | sed -n '/REGION TYPE/,/TOTAL/p'; } >> "$out"
wait $a $b || true
grep -E "^==|Footprint|phys_footprint|TOTAL" "$out" | head -20
