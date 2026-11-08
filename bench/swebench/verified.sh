#!/bin/bash
# SWE-bench Verified, Django and sympy: stage 1 (each task's merged fix,
# with and without foil) picks the tasks that resolve in this environment;
# stage 2 runs Claude Code on them, bare and under `foil agent claude`.
#   FOIL=~/foil-target-041/release/foil bench/swebench/verified.sh ~/swe/verified
# Needs Claude Code on PATH and the API key in ~/.secrets/anthropic_key.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
WORK=${1:-$HOME/swe/verified}
TASKS=$WORK/verified.jsonl
[ -f "$TASKS" ] || python3 "$here/fetch.py" "$TASKS" princeton-nlp/SWE-bench_Verified
cd "$here"
echo "== stage 1: $(date -u +%FT%TZ)"
python3 run.py "$TASKS" "$WORK" --per-version 4 --sympy-per-version 2 --workers 2
echo "== stage 2: $(date -u +%FT%TZ)"
python3 agent.py "$TASKS" "$WORK" "$WORK/results.jsonl" --workers 2
echo "== done: $(date -u +%FT%TZ)"
