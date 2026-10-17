"""Summarize stage 2 (agent.py): resolved rate, cost and time per arm, and foil's own checks.

Usage: agent_summary.py WORKDIR/agent-results.jsonl
"""

import json
import statistics
import sys


def main():
    rows = [json.loads(line) for line in open(sys.argv[1])]
    errors = [r for r in rows if "harness_error" in r]
    rows = [r for r in rows if "harness_error" not in r]
    by = {arm: {r["id"]: r for r in rows if r["arm"] == arm} for arm in ("bare", "foil")}
    both = sorted(set(by["bare"]) & set(by["foil"]))
    print(f"{len(both)} tasks with both arms; {len(errors)} harness errors\n")

    print("| | without foil | with foil (strict) |\n| --- | --- | --- |")
    def col(f):
        return " | ".join(f([by[arm][t] for t in both]) for arm in ("bare", "foil"))

    def resolved(rs):
        return f"{sum(r['resolved'] for r in rs)} / {len(rs)}"

    def median_time(rs):
        return f"{statistics.median(r['agent_s'] for r in rs):.0f} s"

    def median_turns(rs):
        return str(statistics.median(r["turns"] or 0 for r in rs))

    def cost(rs):
        return f"${sum(r['cost_usd'] or 0 for r in rs):.2f}"

    def failures(rs):
        return str(sum(bool(r["agent_error"]) or r["agent_rc"] != 0 for r in rs))

    print(f"| Resolved | {col(resolved)} |")
    print(f"| Median agent time | {col(median_time)} |")
    print(f"| Median turns | {col(median_turns)} |")
    print(f"| Total cost | {col(cost)} |")
    print(f"| Stopped by the 60-turn limit or an error | {col(failures)} |")

    f = [by["foil"][t] for t in both]
    print("\nfoil checks:")
    print(f"- workspace unchanged until commit: {sum(r['ws_unchanged_before_commit'] for r in f)} / {len(f)}")
    print(f"- commit ok: {sum(r['commit_ok'] for r in f)} / {len(f)}")
    print(f"- API key in agent stderr: {sum(r['key_in_stderr'] for r in f)}")
    blocked = sorted({h for r in f for h in r['blocked_hosts']})
    print(f"- hosts the proxy blocked: {', '.join(blocked) or 'none'}")

    agree = sum(by["bare"][t]["resolved"] == by["foil"][t]["resolved"] for t in both)
    only_bare = [t for t in both if by["bare"][t]["resolved"] and not by["foil"][t]["resolved"]]
    only_foil = [t for t in both if by["foil"][t]["resolved"] and not by["bare"][t]["resolved"]]
    print(f"\nsame outcome in both arms: {agree} / {len(both)}")
    print(f"resolved only without foil: {', '.join(only_bare) or 'none'}")
    print(f"resolved only with foil: {', '.join(only_foil) or 'none'}")

    print("\n| Task | without foil | with foil |\n| --- | --- | --- |")
    for t in both:
        cell = lambda r: f"{'yes' if r['resolved'] else 'no'} ({r['agent_s']:.0f} s, {r['turns']} turns)"
        print(f"| {t} | {cell(by['bare'][t])} | {cell(by['foil'][t])} |")
    for e in errors:
        print(f"\nharness error {e['id']} [{e['arm']}]: {e['harness_error'][:300]}")


if __name__ == "__main__":
    main()
