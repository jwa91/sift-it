"""Word error rate against human transcripts: the FLEURS test split (google/fleurs, one language's parquet), every
clip sent to a speech helper as its own chunk.

    uv run --with numpy --with pyarrow --with num2words python fleurs.py run <helper> <model> <test.parquet> <out.jsonl>
    uv run --with num2words python fleurs.py score <out.jsonl>...

`run` writes one line a clip (id, reference, what the helper heard, seconds of sound, seconds it took); `score` gives
corpus WER (all edits over all reference words) after dutch.py's normalization, plain and with compounds merged.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dutch  # noqa: E402
from wer import edits  # noqa: E402


def float_wav(wav_bytes):
    # wave can't read IEEE float: rewrite the format tag so it does, and read the frames as f32.
    import numpy as np

    at = 12
    while at < len(wav_bytes):
        kind, size = wav_bytes[at:at + 4], int.from_bytes(wav_bytes[at + 4:at + 8], "little")
        if kind == b"fmt ":
            tag, channels = int.from_bytes(wav_bytes[at + 8:at + 10], "little"), int.from_bytes(wav_bytes[at + 10:at + 12], "little")
            rate, bits = int.from_bytes(wav_bytes[at + 12:at + 16], "little"), int.from_bytes(wav_bytes[at + 22:at + 24], "little")
        if kind == b"data":
            data = wav_bytes[at + 8:at + 8 + size]
            if rate != 16_000 or channels != 1:
                raise ValueError(f"expected 16 kHz mono, got {rate} Hz × {channels}")
            if tag == 3 and bits == 32:
                return np.frombuffer(data, dtype="<f4")
            if tag == 1 and bits == 16:
                return np.frombuffer(data, dtype="<i2").astype("float32") / 32768
            raise ValueError(f"format {tag}, {bits} bits")
        at += 8 + size + (size & 1)
    raise ValueError("no data")


def run(helper, model, parquet, out):
    import pyarrow.parquet as pq
    from speech import Helper

    rows = pq.read_table(parquet, columns=["id", "audio", "raw_transcription"]).to_pylist()
    speaker = Helper(helper, model)
    audio = busy = 0.0
    with open(out, "w") as sink:
        for row in rows:
            clip = float_wav(row["audio"]["bytes"])
            text, error, took = speaker.run(clip)
            audio += len(clip) / 16_000
            busy += took
            sink.write(json.dumps({"id": row["id"], "reference": row["raw_transcription"], "text": text,
                                   "seconds": len(clip) / 16_000, "took": took, "error": error}) + "\n")
    peak, rss = speaker.close()
    print(json.dumps({"helper": helper, "clips": len(rows), "audio_seconds": audio, "busy_seconds": busy,
                      "rtfx": audio / busy, "load_seconds": speaker.load, "peak_footprint_mb": peak}))


def score(paths):
    for path in paths:
        rows = [json.loads(line) for line in open(path)]
        totals = {"plain": [0, 0, 0, 0], "compounds merged": [0, 0, 0, 0]}
        for row in rows:
            ref, hyp = dutch.words(row["reference"]), dutch.words(row["text"])
            for label, (r, h) in (("plain", (ref, hyp)), ("compounds merged", dutch.merge_compounds(ref, hyp))):
                s, d, i = edits(r, h)
                t = totals[label]
                t[0] += len(r)
                t[1] += s
                t[2] += d
                t[3] += i
        speed = sum(r["seconds"] for r in rows) / sum(r["took"] for r in rows)
        line = f"{os.path.basename(path)}: {len(rows)} clips, {speed:.0f}× real time"
        for label, (n, s, d, i) in totals.items():
            line += f"; {label}: WER {100 * (s + d + i) / n:.2f}% (S {s}, D {d}, I {i} of {n})"
        print(line)


if __name__ == "__main__":
    if sys.argv[1] == "run":
        run(*sys.argv[2:6])
    else:
        score(sys.argv[2:])
