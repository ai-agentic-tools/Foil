"""Summarize repeated stage-2 runs (agent.py --tag): each arm's resolved
count per run, the mean and range, and how stable each task's outcome is.

Usage: repeat_summary.py WORKDIR/agent-results-r1.jsonl WORKDIR/agent-results-r2.jsonl ...
"""

import json
import statistics
import sys

ARMS = ("bare", "foil")


def load(path):
    rows = [json.loads(line) for line in open(path)]
    errors = [r for r in rows if "harness_error" in r]
    by = {arm: {r["id"]: r for r in rows if r["arm"] == arm and "harness_error" not in r} for arm in ARMS}
    return by, errors


def main():
    runs = [load(p) for p in sys.argv[1:]]
    # Only tasks that ran in both arms in every run are compared.
    tasks = sorted(set.intersection(*(set(by[a]) for by, _ in runs for a in ARMS)))
    n = len(tasks)
    print(f"{len(runs)} runs, {n} tasks with both arms in every run, "
          f"{sum(len(e) for _, e in runs)} harness errors\n")

    print("| | " + " | ".join(f"run {i + 1}" for i in range(len(runs))) + " | mean | range |")
    print("| --- |" + " --- |" * (len(runs) + 2))
    for arm, label in (("bare", "without foil"), ("foil", "with foil (strict)")):
        counts = [sum(by[arm][t]["resolved"] for t in tasks) for by, _ in runs]
        print(f"| {label} | " + " | ".join(f"{c} / {n}" for c in counts)
              + f" | {statistics.mean(counts):.1f} | {min(counts)}–{max(counts)} |")
    for arm, label in (("bare", "without foil"), ("foil", "with foil")):
        cost = sum(by[arm][t]["cost_usd"] or 0 for by, _ in runs for t in tasks)
        secs = statistics.median(by[arm][t]["agent_s"] for by, _ in runs for t in tasks)
        stopped = sum(bool(by[arm][t]["agent_error"]) or by[arm][t]["agent_rc"] != 0 for by, _ in runs for t in tasks)
        print(f"\n{label}: total cost ${cost:.2f}, median agent time {secs:.0f} s, "
              f"stopped by the turn limit or an error {stopped} of {n * len(runs)}")

    k = len(runs)
    for arm, label in (("bare", "without foil"), ("foil", "with foil")):
        solved = [sum(by[arm][t]["resolved"] for by, _ in runs) for t in tasks]
        print(f"\n{label}: solved in every run {solved.count(k)}, in some runs {sum(0 < s < k for s in solved)}, "
              f"in none {solved.count(0)}; solved at least once {sum(s > 0 for s in solved)} / {n}")

    f = [by["foil"][t] for by, _ in runs for t in tasks]
    print("\nfoil checks over all runs:")
    print(f"- workspace unchanged until commit: {sum(r['ws_unchanged_before_commit'] for r in f)} / {len(f)}")
    print(f"- commit ok: {sum(r['commit_ok'] for r in f)} / {len(f)}")
    print(f"- API key in agent stderr: {sum(r['key_in_stderr'] for r in f)}")
    blocked = sorted({h for r in f for h in r["blocked_hosts"]})
    print(f"- hosts the proxy blocked: {', '.join(blocked) or 'none'}")

    print("\n| Task | without foil | with foil |\n| --- | --- | --- |")
    for t in tasks:
        cell = lambda arm: "".join("y" if by[arm][t]["resolved"] else "." for by, _ in runs)
        print(f"| {t} | {cell('bare')} | {cell('foil')} |")
    for i, (_, errors) in enumerate(runs):
        for e in errors:
            print(f"\nrun {i + 1} harness error {e['id']} [{e['arm']}]: {e['harness_error'][:300]}")


if __name__ == "__main__":
    main()
