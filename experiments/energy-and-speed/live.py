"""Watch the energy suite as it runs: one line per power sample, and a summary as each workload ends.

    python3 live.py <power.txt> <suite results folder>

Reads what powermetrics has written so far (and keeps reading), the step the suite is on (suite.log beside the
results folder) and the steps it finished (suite.jsonl). A workload's energy is its power above the idle baseline (the
suite's first step) over its own work, without loading the model.
"""
import json
import os
import re
import sys
import time
from datetime import datetime

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from power import FIELDS, HEADER  # noqa: E402

BAR = 40  # columns for 10 W
COLORS = {"cpu": "\033[36m", "gpu": "\033[35m", "ane": "\033[32m"}
RESET, BOLD, DIM = "\033[0m", "\033[1m", "\033[2m"


def parse(chunk):
    """Complete samples in `chunk`, and the unparsed rest."""
    parts = HEADER.split(chunk)
    done, rest = [], chunk
    # parts: [before, when, ms, body, when, ms, body, …]; the last body may be incomplete.
    for i in range(1, len(parts) - 3, 3):
        when, elapsed, body = parts[i], float(parts[i + 1]) / 1000, parts[i + 2]
        end = datetime.strptime(when, "%a %b %d %H:%M:%S %Y %z").timestamp()
        values = {k: int(m.group(1)) / 1000 for k, rx in FIELDS.items() if (m := rx.search(body))}
        pressure = re.search(r"Current pressure level: (\w+)", body)
        done.append((end - elapsed, end, values, pressure.group(1) if pressure else ""))
    if len(parts) >= 4:
        last = chunk.rfind("*** Sampled system activity")
        rest = chunk[last:]
    return done, rest


def bar(values):
    out = ""
    for k in ("cpu", "gpu", "ane"):
        n = round(values.get(k, 0.0) * BAR / 10)
        out += COLORS[k] + "█" * n + RESET
    return out


def energy(power, start, end):
    joules = covered = 0.0
    for s0, s1, values, _ in power:
        overlap = min(end, s1) - max(start, s0)
        if overlap > 0:
            joules += values.get("combined", 0.0) * overlap
            covered += overlap
    return joules, covered


def units(name, out):
    """What the work was, for a per-unit figure: (amount, unit label, scale)."""
    if "passages_per_second" in out:
        return out["texts"] * out.get("rounds", 1), "per 1000 passages", 1000
    if "audio_seconds" in out:
        return out["audio_seconds"], "per hour of audio", 3600
    if "count" in out:
        return out["count"], "per item", 1
    return None


def summarize(mark, power, idle_w):
    outputs = [json.loads(l) for l in mark["output"].splitlines() if l.startswith("{") and '"wall_start"' in l]
    if not outputs:
        joules, covered = energy(power, mark["wall_start"], mark["wall_end"])
        if covered:
            print(f"{BOLD}  {mark['name']}: {joules / covered:.2f} W average{RESET}")
        return
    for out in outputs:
        start, end = out["wall_start"], out["wall_end"]
        joules, covered = energy(power, start, end)
        if not covered:
            continue
        seconds = end - start
        total = joules * seconds / covered
        above = total - idle_w * seconds
        label = mark["name"] + (f"/{out['phase']}" if "phase" in out else "")
        line = f"  {label}: {seconds:.1f} s at {total / seconds:.2f} W, {above:.0f} J above idle"
        per = units(label, out)
        if per:
            amount, unit, scale = per
            line += f" = {above * scale / amount:.1f} J {unit}"
        speed = out.get("passages_per_second") or out.get("rtfx")
        if speed:
            line += f" ({speed:.1f} {'passages/s' if 'passages_per_second' in out else '× real time'})"
        print(f"{BOLD}{line}{RESET}", flush=True)


def main():
    power_path, results = sys.argv[1], sys.argv[2]
    suite_log = os.path.join(os.path.dirname(results.rstrip("/")), "suite.log")
    jsonl = os.path.join(results, "suite.jsonl")
    power, pending, offset, seen = [], "", 0, 0
    idle_w = None
    print(f"{DIM}watching {power_path}; bars: {COLORS['cpu']}CPU{RESET}{DIM} {COLORS['gpu']}GPU{RESET}{DIM} "
          f"{COLORS['ane']}ANE{RESET}{DIM}, {BAR} columns = 10 W{RESET}", flush=True)
    if os.path.exists(power_path):
        # Start from the newest sample: what came before isn't this suite's.
        text = open(power_path, errors="replace").read()
        offset = len(text.encode("utf-8", errors="replace"))
        last = text.rfind("*** Sampled system activity")
        if last >= 0:
            offset = len(text[:last].encode("utf-8", errors="replace"))
    while True:
        if os.path.exists(power_path):
            with open(power_path, errors="replace") as f:
                f.seek(offset)
                chunk = f.read()
                offset = f.tell()
            new, pending = parse(pending + chunk)
            step = "…"
            if os.path.exists(suite_log):
                lines = open(suite_log).read().split("\n")
                step = next((l.split(" ", 1)[1] for l in reversed(lines) if l.strip()), "…")
            for sample in new:
                power.append(sample)
                _, end, values, pressure = sample
                print(f"{time.strftime('%H:%M:%S', time.localtime(end))} {step[:26]:26} "
                      f"CPU {values.get('cpu', 0):5.2f}  GPU {values.get('gpu', 0):5.2f}  ANE {values.get('ane', 0):5.2f}"
                      f"  = {values.get('combined', 0):5.2f} W  {bar(values)}"
                      f"{'  ' + pressure if pressure and pressure != 'Nominal' else ''}", flush=True)
        if os.path.exists(jsonl):
            marks = [json.loads(l) for l in open(jsonl) if l.strip()]
            for mark in marks[seen:]:
                if mark["name"] == "idle-start":
                    joules, covered = energy(power, mark["wall_start"], mark["wall_end"])
                    idle_w = joules / covered if covered else 0.0
                    print(f"{BOLD}  idle baseline: {idle_w:.2f} W{RESET}", flush=True)
                else:
                    summarize(mark, power, idle_w or 0.0)
            seen = len(marks)
        time.sleep(1)


if __name__ == "__main__":
    main()
