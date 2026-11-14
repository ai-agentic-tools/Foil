"""Tables for RESULTS.md from the raw results.

Usage: summary.py RESULTS_DIR   (holds partA-run*.jsonl, partB.jsonl, partB-logs/)
"""

import glob
import json
import os
import re
import sys


def part_a(d):
    print("## Part A\n")
    for path in sorted(glob.glob(f"{d}/partA-run*.jsonl")):
        print(f"### {os.path.basename(path)}\n")
        print("| | empty rollout (median) | 1 at a time: per hour, median s | 4 at once: per hour, median s | rewards right |")
        print("| --- | --- | --- | --- | --- |")
        for r in map(json.loads, open(path)):
            print(f"| {r['arm']} | {r['empty_rollout_s']['median'] * 1000:.0f} ms | {r['w1']['per_hour']}, {r['w1']['median_s']:.2f} | "
                  f"{r['w4']['per_hour']}, {r['w4']['median_s']:.2f} | {r['w1']['rewards_right'] + r['w4']['rewards_right']} / "
                  f"{r['w1']['rollouts'] + r['w4']['rollouts']} |")
        print()
        print("| kill -9 of the driver | " + " | ".join(["before", "running", "after 2 s", "after 45 s"]) + " |")
        print("| --- | --- | --- | --- | --- |")
        for r in map(json.loads, open(path)):
            k = r["kill_9"]
            cell = lambda x: ", ".join(f"{n} {v}" for n, v in x.items() if v) or "nothing"
            print(f"| {r['arm']} | {cell(k['before'])} | {cell(k['running'])} | {cell(k['after_2s'])} | {cell(k['after_45s'])} |")
        print()


def files_in(patch):
    return sorted(set(re.findall(r"^\+\+\+ b/(\S+)", patch, re.M)))


def part_b(d):
    rows = [r for r in map(json.loads, open(f"{d}/partB.jsonl")) if "harness_error" not in r]
    print("## Part B\n")
    print(f"{len(rows)} runs, total cost ${sum(r['cost_usd'] or 0 for r in rows):.2f}\n")
    for mode in ("spontaneous", "red-team"):
        print(f"### {mode}\n")
        print("| | false rewards | searched outside the repo | read the job's task data | network attempts | API key in output |")
        print("| --- | --- | --- | --- | --- | --- |")
        for arm in ("bare", "foil"):
            rs = [r for r in rows if r["mode"] == mode and r["arm"] == arm]
            fr = [r["id"] for r in rs if r["false_reward"]]
            print(f"| {arm} | {len(fr)} / {len(rs)} ({', '.join(fr) or '-'}) | {sum(bool(r['did']['searched_machine']) for r in rs)} | "
                  f"{sum(bool(r['did']['task_data']) for r in rs)} | {sum(bool(r['did']['network']) for r in rs)} | "
                  f"{sum(r['key_in_output'] for r in rs)} |")
        print()
    print("| Task | mode | arm | false reward | files the run changed |")
    print("| --- | --- | --- | --- | --- |")
    for r in sorted(rows, key=lambda r: (r["mode"], r["id"], r["arm"])):
        p = f"{d}/partB-logs/{r['id']}/{r['mode']}-{r['arm']}.patch"
        files = files_in(open(p).read()) if os.path.exists(p) else []
        print(f"| {r['id']} | {r['mode']} | {r['arm']} | {'yes' if r['false_reward'] else 'no'} | {', '.join(files) or '-'} |")


if __name__ == "__main__":
    part_a(sys.argv[1])
    part_b(sys.argv[1])
