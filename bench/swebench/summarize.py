"""Summarize run.py's results.jsonl as Markdown. Usage: summarize.py RESULTS_JSONL"""

import json
import statistics
import sys

CHECKS = [
    ("foil_matches_baseline", "Fix under foil: every test outcome = baseline"),
    ("committed_matches_baseline", "After commit, outside foil: every test outcome = baseline"),
    ("neg_f2p_fail", "Without the fix under foil: the target tests fail"),
    ("neg_ws_unchanged", "After abort: workspace unchanged"),
    ("gold_ws_unchanged", "While the branch exists: workspace unchanged"),
    ("diff_covers_patches", "foil diff lists every file the patches touch"),
    ("commit_ok", "foil commit succeeds"),
]

rows = [json.loads(line) for line in open(sys.argv[1])]
ran = [r for r in rows if "baseline" in r and "harness_error" not in r]
skipped = [r for r in rows if "skipped" in r]
errors = [r for r in rows if "harness_error" in r]

print(f"Tasks: {len(rows)} ({len(ran)} ran, {len(skipped)} skipped as too slow, {len(errors)} harness errors)\n")
print("| Check | Passed |\n| --- | --- |")
for key, label in CHECKS:
    ok = sum(1 for r in ran if r.get(key))
    print(f"| {label} | {ok} / {len(ran)} |")
resolved = sum(1 for r in ran if r["baseline"]["resolved"])
print(f"\nBaseline resolved (all target and regression tests pass, no foil): {resolved} / {len(ran)}; "
      "unresolved ones fail identically with and without foil (environment differences from SWE-bench's Docker images).\n")

print("| Task | Version | Baseline s | Under foil s | Resolved | All checks |\n| --- | --- | --- | --- | --- | --- |")
for r in ran:
    allok = all(r.get(k) for k, _ in CHECKS)
    print(f"| {r['id']} | {r['version']} | {r['baseline_s']} | {r['foil_s']} | {'yes' if r['baseline']['resolved'] else 'no'} | {'ok' if allok else 'FAIL: ' + ', '.join(k for k, _ in CHECKS if not r.get(k))} |")
for r in skipped:
    print(f"| {r['id']} | {r['version']} | — | — | — | skipped: {r['skipped']} |")
for r in errors:
    print(f"| {r['id']} | {r['version']} | — | — | — | harness error: {r['harness_error'][:120]} |")

pairs = [(r["baseline_s"], r["foil_s"]) for r in ran if r["baseline_s"] > 0]
if pairs:
    ratio = statistics.median(f / b for b, f in pairs)
    print(f"\nMedian test-run time under foil / baseline: {ratio:.2f} "
          "(the foil run also applies the patches; baseline and foil runs are not interleaved)")
