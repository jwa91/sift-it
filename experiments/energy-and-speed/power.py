"""Energy per workload: powermetrics samples cut by the suite's wall-clock marks.

    python3 power.py <power.txt> <suite.jsonl>

power.txt is `sudo powermetrics --samplers cpu_power,gpu_power,ane_power,thermal -i 1000 -o power.txt` (text form).
Each sample covers the `elapsed` ms before its header's time; a workload gets each sample's power times the part of
it that falls in the workload. Above idle subtracts the idle-start power (the same machine doing nothing), which is
what the work itself costs.
"""
import json
import re
import sys
from datetime import datetime

HEADER = re.compile(r"\*\*\* Sampled system activity \((.+?)\) \(([\d.]+)ms elapsed\) \*\*\*")
FIELDS = {
    "cpu": re.compile(r"^CPU Power: (\d+) mW", re.M),
    "gpu": re.compile(r"^GPU Power: (\d+) mW", re.M),
    "ane": re.compile(r"^ANE Power: (\d+) mW", re.M),
    "combined": re.compile(r"^Combined Power \(CPU \+ GPU \+ ANE\): (\d+) mW", re.M),
}
PRESSURE = re.compile(r"Current pressure level: (\w+)")


def samples(path):
    text = open(path, errors="replace").read()
    parts = HEADER.split(text)
    for i in range(1, len(parts) - 2, 3):
        when, elapsed, body = parts[i], float(parts[i + 1]) / 1000, parts[i + 2]
        end = datetime.strptime(when, "%a %b %d %H:%M:%S %Y %z").timestamp()
        values = {k: int(m.group(1)) / 1000 for k, rx in FIELDS.items() if (m := rx.search(body))}
        pressure = PRESSURE.search(body)
        yield end - elapsed, end, values, pressure.group(1) if pressure else None


def main():
    power = list(samples(sys.argv[1]))
    marks = []
    for line in open(sys.argv[2]):
        mark = json.loads(line)
        # A bench's own marks (its work, without loading the model) stand in for the step's, one per phase.
        inner = [json.loads(l) for l in mark["output"].splitlines() if l.startswith("{") and '"wall_start"' in l]
        if not inner:
            marks.append(mark)
        for i, out in enumerate(inner):
            name = mark["name"] + (f"/{out['phase']}" if "phase" in out else "")
            marks.append({"name": name, "wall_start": out["wall_start"], "wall_end": out["wall_end"],
                          "output": json.dumps(out)})
    results = {}
    for mark in marks:
        start, end = mark["wall_start"], mark["wall_end"]
        energy = {k: 0.0 for k in FIELDS}
        covered, pressures = 0.0, set()
        for s0, s1, values, pressure in power:
            overlap = min(end, s1) - max(start, s0)
            if overlap <= 0:
                continue
            covered += overlap
            pressures.add(pressure)
            for k in FIELDS:
                energy[k] += values.get(k, 0.0) * overlap
        seconds = end - start
        results[mark["name"]] = {"seconds": seconds, "covered": covered, "joules": energy, "pressure": pressures,
                                 "output": mark["output"]}
    idle = results.get("idle-start")
    idle_watts = idle["joules"]["combined"] / idle["covered"] if idle and idle["covered"] else 0.0
    print(f"idle: {idle_watts:.2f} W (CPU+GPU+ANE)")
    print(f"{'workload':28} {'s':>7} {'CPU W':>6} {'GPU W':>6} {'ANE W':>6} {'all W':>6} {'J':>8} {'J>idle':>8}  pressure")
    for name, r in results.items():
        if not r["covered"]:
            print(f"{name:28} no power samples")
            continue
        w = {k: v / r["covered"] for k, v in r["joules"].items()}
        total = r["joules"]["combined"] * r["seconds"] / r["covered"]
        above = total - idle_watts * r["seconds"]
        r["total_j"], r["above_idle_j"] = total, above
        print(f"{name:28} {r['seconds']:7.1f} {w['cpu']:6.2f} {w['gpu']:6.2f} {w['ane']:6.2f} {w['combined']:6.2f} "
              f"{total:8.1f} {above:8.1f}  {','.join(sorted(p for p in r['pressure'] if p))}")
    print(json.dumps({k: {kk: vv for kk, vv in v.items() if kk not in ("output", "pressure")} for k, v in results.items()}),
          file=open(sys.argv[2].replace(".jsonl", "-energy.json"), "w"))


if __name__ == "__main__":
    main()
