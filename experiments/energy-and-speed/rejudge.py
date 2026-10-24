"""The spans disagree.py sampled, read again in a new transcript by the second engine: what it now says where the
first engine's words stand (the first engine's transcript, and the chunks, unchanged).

    uv run --with num2words python rejudge.py spans.txt.json judged.json a.jsonl new_b.jsonl [old_b.jsonl [count]]

With old_b, it also lists `count` (default 20) of the new disagreements: spans between a and new_b where a and old_b
agreed. judged.json maps each span's number (1-based) to how it was judged: format, filler, a-error, b-error or unclear.
Prints, by kind, how many spans the new transcript now agrees on, still says the same, or says differently, and lists
the ones to judge again.
"""
import collections
import difflib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dutch  # noqa: E402


def main():
    spans_path, judged_path, a_path, b_path = sys.argv[1:5]
    spans = json.load(open(spans_path))
    judged = json.load(open(judged_path))
    a = [json.loads(line) for line in open(a_path)]
    b = [json.loads(line) for line in open(b_path)]
    counts = collections.Counter()
    for k, span in enumerate(spans, 1):
        x, y = a[span["chunk"]], b[span["chunk"]]
        wa, wb = dutch.merge_compounds(dutch.words(x["text"]), dutch.words(y["text"]))
        i1, i2 = span["i1"], span["i2"]
        picked = []
        for tag, a1, a2, b1, b2 in difflib.SequenceMatcher(None, wa, wb, autojunk=False).get_opcodes():
            if tag == "equal":
                lo, hi = max(a1, i1), min(a2, i2)
                picked += wb[b1 + lo - a1:b1 + hi - a1] if lo < hi else []
            elif (a1 < i2 and a2 > i1) or (a1 == a2 and i1 <= a1 <= i2) or (i1 == i2 and a1 <= i1 <= a2):
                picked += wb[b1:b2]
        new, old = " ".join(picked), span["b"]
        kind = judged[str(k)]
        outcome = "same as before" if new == old else "agrees now" if new == span["a"] else "different"
        counts[(kind, outcome)] += 1
        if outcome != "same as before":
            print(f"{k:3d} {kind:8} {outcome:11} A: {span['a'] or '∅'} | was: {old or '∅'} | now: {new or '∅'}")
    for kind in ("b-error", "a-error", "format", "filler", "unclear"):
        row = {o: counts[(kind, o)] for o in ("same as before", "agrees now", "different")}
        print(f"{kind:8} {row}")
    if len(sys.argv) > 5:
        new_spans(a, b, [json.loads(line) for line in open(sys.argv[5])], int(sys.argv[6]) if len(sys.argv) > 6 else 20)


def differing(wa, wb):
    return [(a1, a2, b1, b2) for tag, a1, a2, b1, b2 in difflib.SequenceMatcher(None, wa, wb, autojunk=False).get_opcodes()
            if tag != "equal"]


def new_spans(a, b, old, count):
    found = []
    for x, y, z in zip(a, b, old):
        wa = dutch.words(x["text"])
        pa, pb = dutch.merge_compounds(wa, dutch.words(y["text"]))
        qa, qb = dutch.merge_compounds(wa, dutch.words(z["text"]))
        before = set()
        for a1, a2, _, _ in differing(qa, qb):
            before.update(range(a1 - 1, a2 + 1))
        for a1, a2, b1, b2 in differing(pa, pb):
            if not before & set(range(a1, max(a2, a1 + 1))):
                found.append(f"…{' '.join(pa[max(0, a1 - 6):a1])} [A: {' '.join(pa[a1:a2]) or '∅'} | "
                             f"new B: {' '.join(pb[b1:b2]) or '∅'}] {' '.join(pa[a2:a2 + 6])}…")
    print(f"{len(found)} new disagreements")
    step = max(1, len(found) // count)
    for line in found[::step][:count]:
        print("   ", line)


if __name__ == "__main__":
    main()
