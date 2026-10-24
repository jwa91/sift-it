"""Where two transcripts of the same Dutch chunks disagree (both from speech.py over the same sound), after dutch.py's
normalization: their WER-style distance, and the spans where they differ, with context, to judge by hand.

    uv run --with num2words python disagree.py a.jsonl b.jsonl [spans.txt [count]]

Neither side is ground truth. Transcripts cut into different chunks are compared file by file. A span is a run of
words that differ between the two, with the words around it. `spans.txt` gets `count` of them (default 60), spread
evenly over the recording.
"""
import difflib
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dutch  # noqa: E402
from wer import edits  # noqa: E402

CONTEXT = 6


def main():
    a_path, b_path, *rest = sys.argv[1:]
    a = [json.loads(line) for line in open(a_path)]
    b = [json.loads(line) for line in open(b_path)]
    if [(x["file"], x["start"]) for x in a] != [(x["file"], x["start"]) for x in b]:
        # Cut differently: compare each file's whole transcript instead.
        def joined(rows):
            files = {}
            for row in rows:
                files.setdefault(row["file"], []).append(row["text"])
            return [{"file": f, "start": 0.0, "text": " ".join(t)} for f, t in files.items()]

        a, b = joined(a), joined(b)
        assert [x["file"] for x in a] == [x["file"] for x in b], "different files"
    totals = {"all words": [0, 0], "without fillers": [0, 0]}
    spans = []
    for x, y in zip(a, b):
        wa, wb = dutch.merge_compounds(dutch.words(x["text"]), dutch.words(y["text"]))
        for label, (p, q) in (("all words", (wa, wb)),
                              ("without fillers", ([w for w in wa if w not in dutch.FILLERS],
                                                   [w for w in wb if w not in dutch.FILLERS]))):
            totals[label][0] += len(p)
            totals[label][1] += sum(edits(p, q))
        for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, wa, wb, autojunk=False).get_opcodes():
            if tag == "equal":
                continue
            spans.append({"file": os.path.basename(x["file"]), "at": x["start"],
                          "before": " ".join(wa[max(0, i1 - CONTEXT):i1]), "a": " ".join(wa[i1:i2]),
                          "b": " ".join(wb[j1:j2]), "after": " ".join(wa[i2:i2 + CONTEXT])})
    for label, (n, e) in totals.items():
        print(f"{label}: {n} words, {e} edits, WER {100 * e / n:.2f}%")
    print(f"{len(spans)} differing spans")
    if rest:
        count = int(rest[1]) if len(rest) > 1 else 60
        step = max(1, len(spans) // count)
        with open(rest[0], "w") as out:
            for k, span in enumerate(spans[::step][:count], 1):
                out.write(f"{k:3d} [{span['file']} {span['at']:.0f}s] …{span['before']} "
                          f"[A: {span['a'] or '∅'} | B: {span['b'] or '∅'}] {span['after']}…\n")


if __name__ == "__main__":
    main()
