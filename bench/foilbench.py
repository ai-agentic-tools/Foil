#!/usr/bin/env python3
"""foilbench: foil's speed, measured the same way every time (family J of the quality plan).

Measures, with the binary given by $FOIL (default: foil on PATH):

  spawn_true          `true` started directly: the machine's own process cost (calibration)
  run_plain           foil run -- true
  branch_start        foil run -b NEW -- true      (a new branch per run)
  branch_start_strict foil run --strict -b NEW -- true
  abort               foil abort NAME
  commit_1            foil commit, 1 changed file
  commit_1000         foil commit, 1000 changed files
  branch_start_100k   branch_start in a workspace of 100 000 files (must stay flat)
  copy_up_100mb       append 1 byte to a 100 MB file inside a branch (overlay copies it up)
  concurrent_N        N branch starts at once (N = 10, 50): p50/p99 of each one's latency

Usage:
  foilbench.py [--quick] [--out results.json]
  foilbench.py --gate BASELINE.json [--tolerance 0.10] RESULTS.json
      fails (exit 1) if a metric's median is more than `tolerance` slower than the baseline
      after scaling both by their own spawn_true (so a slower machine is not a regression).
"""

import argparse
import concurrent.futures
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

FOIL = os.environ.get("FOIL", "foil")

# Goals from the quality plan; reported, not enforced.
TARGETS_MS = {"branch_start": 5.0, "abort": 1.0}


def timed(cmd, cwd=None, env=None):
    start = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    took = (time.perf_counter() - start) * 1000
    if p.returncode != 0:
        raise RuntimeError(f"{cmd} failed: {p.stderr.decode()[-500:]}")
    return took


def summary(samples):
    s = sorted(samples)
    return {"p50": round(statistics.median(s), 3), "p99": round(s[min(len(s) - 1, int(len(s) * 0.99))], 3), "n": len(s)}


class Bench:
    def __init__(self, quick):
        self.n = 15 if quick else 60
        self.root = tempfile.mkdtemp(prefix="foilbench-")
        self.env = dict(os.environ, FOIL_HOME=os.path.join(self.root, "home"))
        self.counter = 0

    def ws(self, name, files=10):
        path = os.path.join(self.root, name)
        if not os.path.isdir(path):
            os.makedirs(path)
            for i in range(files):
                d = os.path.join(path, f"d{i // 1000}")
                os.makedirs(d, exist_ok=True)
                with open(os.path.join(d, f"f{i}"), "w") as f:
                    f.write("x\n")
        return path

    def fresh(self):
        self.counter += 1
        return f"b{self.counter}"

    def run(self):
        ws = self.ws("ws")
        r = {}
        r["spawn_true"] = summary([timed(["true"]) for _ in range(self.n)])
        r["run_plain"] = summary([timed([FOIL, "run", "--", "true"], ws, self.env) for _ in range(self.n)])
        r["branch_start"] = summary([timed([FOIL, "run", "-b", self.fresh(), "--", "true"], ws, self.env) for _ in range(self.n)])
        try:
            r["branch_start_strict"] = summary(
                [timed([FOIL, "run", "--strict", "-b", self.fresh(), "--", "true"], ws, self.env) for _ in range(self.n)])
        except RuntimeError as err:
            print(f"skipping strict: {err}", file=sys.stderr)

        names = []
        for _ in range(self.n):
            b = self.fresh()
            timed([FOIL, "run", "-b", b, "--", "sh", "-c", "echo y > d0/f0"], ws, self.env)
            names.append(b)
        r["abort"] = summary([timed([FOIL, "abort", b], ws, self.env) for b in names])

        samples = []
        for i in range(self.n):
            b = self.fresh()
            timed([FOIL, "run", "-b", b, "--", "sh", "-c", f"echo {i} > c1"], ws, self.env)
            samples.append(timed([FOIL, "commit", b], ws, self.env))
        r["commit_1"] = summary(samples)

        samples = []
        for i in range(max(3, self.n // 10)):
            b = self.fresh()
            timed([FOIL, "run", "-b", b, "--", "sh", "-c", f"mkdir -p many && for j in $(seq 1000); do echo {i} > many/$j; done"], ws, self.env)
            samples.append(timed([FOIL, "commit", b], ws, self.env))
        r["commit_1000"] = summary(samples)

        big = self.ws("ws100k", files=100_000)
        r["branch_start_100k"] = summary([timed([FOIL, "run", "-b", self.fresh(), "--", "true"], big, self.env) for _ in range(self.n)])

        with open(os.path.join(ws, "big"), "wb") as f:
            f.truncate(0)
            f.write(os.urandom(1 << 20) * 100)
        samples = []
        for _ in range(max(3, self.n // 10)):
            samples.append(timed([FOIL, "run", "-b", self.fresh(), "--", "sh", "-c", "printf x >> big"], ws, self.env))
        r["copy_up_100mb"] = summary(samples)

        for n in (10, 50):
            with concurrent.futures.ThreadPoolExecutor(n) as pool:
                lat = list(pool.map(lambda b: timed([FOIL, "run", "-b", b, "--", "true"], ws, self.env),
                                    [self.fresh() for _ in range(n * 3)]))
            r[f"concurrent_{n}"] = summary(lat)
        return r

    def close(self):
        subprocess.run(["chmod", "-R", "u+rwx", self.root])
        shutil.rmtree(self.root, ignore_errors=True)


def gate(baseline, results, tolerance):
    base_cal, now_cal = baseline["spawn_true"]["p50"], results["spawn_true"]["p50"]
    scale = now_cal / base_cal if base_cal > 0 else 1.0
    worse = []
    for k, v in baseline.items():
        if k == "spawn_true" or k.startswith("_") or k not in results:
            continue
        limit = v["p50"] * max(scale, 1.0) * (1 + tolerance)
        mark = "REGRESSION" if results[k]["p50"] > limit else "ok"
        print(f"{k:22} baseline {v['p50']:9.2f} ms   now {results[k]['p50']:9.2f} ms   limit {limit:9.2f}   {mark}")
        if mark != "ok":
            worse.append(k)
    return worse


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--out")
    ap.add_argument("--gate")
    ap.add_argument("--tolerance", type=float, default=0.10)
    ap.add_argument("results", nargs="?")
    a = ap.parse_args()
    if a.gate:
        worse = gate(json.load(open(a.gate)), json.load(open(a.results)), a.tolerance)
        sys.exit(1 if worse else 0)
    b = Bench(a.quick)
    try:
        r = b.run()
    finally:
        b.close()
    r["_meta"] = {"foil": subprocess.run([FOIL, "--version"], capture_output=True, text=True).stdout.strip(),
                  "kernel": os.uname().release, "time": time.strftime("%Y-%m-%dT%H:%M:%S")}
    for k, v in r.items():
        if k.startswith("_"):
            continue
        target = TARGETS_MS.get(k)
        goal = f"  (goal < {target} ms: {'met' if v['p50'] < target else 'not met'})" if target else ""
        print(f"{k:22} p50 {v['p50']:9.2f} ms   p99 {v['p99']:9.2f} ms   n={v['n']}{goal}")
    if a.out:
        json.dump(r, open(a.out, "w"), indent=1)


if __name__ == "__main__":
    main()
