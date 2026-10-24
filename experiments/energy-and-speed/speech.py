"""Times a speech helper on the same chunks of sound sift-it sends it, and writes what it heard.

    uv run --with numpy python speech.py <helper> <model> <out.jsonl> <sound.wav>...

The helper is `sift-it-transcribe` (transcribe.cpp) or `fluid-transcribe` (FluidAudio): both read chunks on stdin (a
little-endian u32 sample count, then f32 samples, 16 kHz mono) and answer with one JSON line each. The WAVs are 16 kHz
mono float (`afconvert -f WAVE -d LEF32@16000 -c 1 in.mp3 out.wav`). Chunks are cut the way ingest/extract/speech.rs
cuts them: about three minutes, at the quietest tenth of a second within ten seconds of the mark; a silent chunk isn't
sent. The helper runs under /usr/bin/time -l, for its peak memory.
"""
import json
import struct
import subprocess
import sys
import time
import wave

import numpy as np

RATE = 16_000
CHUNK = 180 * RATE
CUT_WINDOW = 10 * RATE
SILENCE = 0.003


def rms(x):
    return float(np.sqrt(np.mean(np.square(x, dtype=np.float64)))) if len(x) else 0.0


def quiet_point(samples, mark):
    frame = RATE // 10
    lo, hi = max(mark - CUT_WINDOW, 0), min(mark + CUT_WINDOW, len(samples))
    best = (float("inf"), mark)
    at = lo
    while at + frame <= hi:
        level = rms(samples[at:at + frame])
        if level < best[0]:
            best = (level, at + frame // 2)
        at += frame
    return min(max(best[1], 1), len(samples))


def chunks(samples):
    start = 0
    while len(samples) - start >= CHUNK + CUT_WINDOW:
        cut = start + quiet_point(samples[start:start + CHUNK + CUT_WINDOW], CHUNK)
        yield start, samples[start:cut]
        start = cut
    if len(samples) - start >= RATE // 2:
        yield start, samples[start:]


def read_wav(path):
    with open(path, "rb") as f:
        data = f.read()
    # A float WAV: find the data chunk (wave can't read IEEE float).
    at = 12
    while at < len(data):
        kind, size = data[at:at + 4], struct.unpack("<I", data[at + 4:at + 8])[0]
        if kind == b"data":
            return np.frombuffer(data[at + 8:at + 8 + size], dtype="<f4")
        at += 8 + size + (size & 1)
    raise ValueError(f"no data in {path}")


def main():
    helper, model, out, *files = sys.argv[1:]
    started = time.time()
    proc = subprocess.Popen(["/usr/bin/time", "-l", helper, model], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    ready = json.loads(proc.stdout.readline())
    load = time.time() - started
    wall_start = time.time()
    audio = busy = 0.0
    with open(out, "w") as sink:
        for path in files:
            samples = read_wav(path)
            for start, chunk in chunks(samples):
                seconds = len(chunk) / RATE
                audio += seconds
                if rms(chunk) < SILENCE:
                    continue
                t = time.time()
                proc.stdin.write(struct.pack("<I", len(chunk)) + chunk.astype("<f4").tobytes())
                proc.stdin.flush()
                answer = json.loads(proc.stdout.readline())
                took = time.time() - t
                busy += took
                text = " ".join(s["text"] for s in answer.get("segments", []))
                sink.write(json.dumps({"file": path, "start": start / RATE, "seconds": seconds, "took": took,
                                       "text": text, "error": answer.get("error")}) + "\n")
    wall_end = time.time()
    proc.stdin.close()
    proc.wait()
    stats = proc.stderr.read().decode()
    peak = next((int(l.split()[0]) for l in stats.splitlines() if "peak memory footprint" in l), None)
    rss = next((int(l.split()[0]) for l in stats.splitlines() if "maximum resident set size" in l), None)
    print(json.dumps({"helper": helper, "ready": ready, "load_seconds": load, "audio_seconds": audio,
                      "busy_seconds": busy, "rtfx": audio / busy, "peak_footprint_mb": peak and peak / 2**20,
                      "max_rss_mb": rss and rss / 2**20, "wall_start": wall_start, "wall_end": wall_end}))


if __name__ == "__main__":
    main()
