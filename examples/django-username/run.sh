#!/bin/sh
# Three Claude Code sessions fix Django ticket #30257 at the same time, each
# in a strict foil branch of the same checkout. foil runs check.py in each
# branch, commits the smallest passing fix, and discards the rest.
#   ANTHROPIC_API_KEY=... ./run.sh     (after ./setup.sh; N=3, MODEL=haiku)
set -eu
FOIL=${FOIL:-foil}
WORK=${WORK:-$HOME/foil-django-demo}
here=$(cd "$(dirname "$0")" && pwd)
: "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY (a workspace API key; the foil proxy keeps it from the agents)}"
[ -x "$WORK/env/bin/python" ] || { echo "run ./setup.sh first" >&2; exit 1; }
PY=$WORK/env/bin/python
export FOIL_HOME=$WORK/foil-home
cd "$WORK/django"
git checkout -q -- . && git clean -qfd     # start from the bug every time

prompt="You are fixing an issue in the django/django repository, checked out in the current folder.

<issue>
$(cat "$here/issue.txt")
</issue>

Change the source code so the issue is fixed. Do not edit or add tests; hidden tests will check your fix.
Python with Django's dependencies: $PY
Existing tests: cd tests && PYTHONPATH=.. $PY runtests.py --settings=test_sqlite --parallel 1 auth_tests
When you are done, reply with one line summarizing the change."

# The agents may read the Python environment (and the interpreter uv
# installed for it); check.py is shown only to the checks.
started=$(date +%s)
"$FOIL" fanout -n "${N:-3}" --name username --agent claude --timeout 10m --json \
    --allow-read "$WORK/env" --allow-read "$(uv python dir)" --allow-read "$here/check.py" \
    --check "$PY -B $here/check.py" -- \
    --model "${MODEL:-haiku}" --max-turns 30 --permission-mode acceptEdits \
    --allowedTools "Bash,Read,Edit,Write,Glob,Grep" -p "$prompt" > "$WORK/result.json" || true
echo "($(($(date +%s) - started)) s in all)"
best=$("$PY" -c 'import json, sys; print(json.load(open(sys.argv[1]))["best"] or "")' "$WORK/result.json")
logs=$FOIL_HOME/fanout/username

printf '\n== What each agent said it did\n'
for log in "$logs"/attempt-*.log; do
    printf '%s: ' "$(basename "$log" .log)"
    awk '/^foil: the agent/ {exit} NF {last=$0} END {print last}' "$log"
done
printf '\n== What each agent could reach (the same sandbox, probed)\n'
blocked=$(cat "$logs"/*.log | grep "blocked network access" | sort -u)
[ -z "$blocked" ] || echo "$blocked"
"$FOIL" run -b probe --strict --allow-host api.anthropic.com --inject-key anthropic \
    --allow-read "$WORK/env" --allow-read "$(uv python dir)" -- sh -c '
    echo "API key it sees:  $ANTHROPIC_API_KEY"
    echo "home folder:      $(ls -A ~ | tr "\n" " ")(workspace, Python, tools on PATH)"
    printf "~/.ssh:           "; ls ~/.ssh 2>&1
    echo "processes:        $(ls /proc | grep -c "^[0-9]") visible"
    printf "example.com:      "; '"$PY"' -c "import urllib.request as u; u.urlopen(\"https://example.com\", timeout=5)" 2>&1 | tail -1' \
    2>/dev/null
"$FOIL" abort probe

[ -n "$best" ] || { echo "no attempt passed the check; branches kept: $FOIL list"; exit 1; }
printf '\n== The best attempt, %s, before it touches the checkout\n' "$best"
git status --short; echo "(checkout clean)"
"$FOIL" diff --patch "$best"

printf '\n== foil pick %s\n' "$best"
"$FOIL" pick "$best"
printf '\n== Django, fixed (check.py outside foil)\n'
"$PY" -B "$here/check.py"
