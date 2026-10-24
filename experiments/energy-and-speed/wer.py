"""How far two transcripts of the same chunks are apart: word error rate of `b` against `a`, per file group.

    python3 wer.py a.jsonl b.jsonl [prefix=label ...]

Both files come from speech.py over the same sound, so their lines pair up chunk by chunk. Words are compared after
lower-casing and dropping punctuation, so "Hagelslag," and "hagelslag" agree. Neither side is ground truth: this
measures disagreement (substitutions + insertions + deletions over a's words).
"""
import json
import re
import sys
import unicodedata


def words(text):
    text = unicodedata.normalize("NFKC", text).lower()
    text = re.sub(r"[^\w\s']|_", " ", text)
    return text.split()


def edits(a, b):
    """Substitutions, deletions, insertions turning a into b (word-level Levenshtein with backtrace counts)."""
    prev = [(j, 0, 0, j) for j in range(len(b) + 1)]  # (cost, subs, dels, ins)
    for i in range(1, len(a) + 1):
        cur = [(i, 0, i, 0)]
        for j in range(1, len(b) + 1):
            if a[i - 1] == b[j - 1]:
                best = prev[j - 1]
            else:
                s, d, n = prev[j - 1], prev[j], cur[j - 1]
                best = min((s[0] + 1, s[1] + 1, s[2], s[3]), (d[0] + 1, d[1], d[2] + 1, d[3]),
                           (n[0] + 1, n[1], n[2], n[3] + 1))
            cur.append(best)
        prev = cur
    return prev[-1][1:]


def main():
    a_path, b_path, *labels = sys.argv[1:]
    groups = dict(label.split("=", 1) for label in labels) or {"": "all"}
    a = [json.loads(line) for line in open(a_path)]
    b = [json.loads(line) for line in open(b_path)]
    assert [(x["file"], x["start"]) for x in a] == [(x["file"], x["start"]) for x in b], "different chunks"
    totals = {}
    for x, y in zip(a, b):
        group = next((label for prefix, label in groups.items() if prefix in x["file"]), "other")
        wa, wb = words(x["text"]), words(y["text"])
        s, d, i = edits(wa, wb)
        t = totals.setdefault(group, [0, 0, 0, 0, 0])
        t[0] += len(wa)
        t[1] += s
        t[2] += d
        t[3] += i
        t[4] += len(wb)
    for group, (n, s, d, i, nb) in totals.items():
        print(f"{group}: {n} words vs {nb}; WER {100 * (s + d + i) / max(n, 1):.2f}% "
              f"(substitutions {s}, deletions {d}, insertions {i})")


if __name__ == "__main__":
    main()
