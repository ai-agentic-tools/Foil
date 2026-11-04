#!/usr/bin/env bash
# The README's screencast: the same run as run.sh, typed out as you would.
# Every command shown is the command run; the variables are set below.
#   asciinema rec -c ./screencast.sh demo.cast && agg demo.cast demo.gif
# Needs ANTHROPIC_API_KEY and ./setup.sh, like run.sh.
set -eu
WORK=${WORK:-$HOME/foil-django-demo}
here=$(cd "$(dirname "$0")" && pwd)
: "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY}"
export FOIL_HOME=$WORK/foil-home GIT_PAGER=cat PAGER=cat
cd "$WORK/django"
git checkout -q -- . && git clean -qfd
for b in $(foil list 2>/dev/null | awk 'NR > 1 {print $1}'); do foil abort "$b" >/dev/null; done

PY=$WORK/env/bin/python
# What the sandbox may read besides the checkout: the Python and the check.
SANDBOX="--allow-read $WORK/env --allow-read $(uv python dir) --allow-read $here/check.py"
CHECK="$PY -B $here/check.py"   # upstream's tests for the fix
CLAUDE="--model haiku --max-turns 30 --permission-mode acceptEdits --allowedTools Bash,Read,Edit,Write,Glob,Grep"
TASK="You are fixing an issue in the django/django repository, checked out in the current folder.
<issue>
$(cat "$here/issue.txt")
</issue>
Change the source code so the issue is fixed. Do not edit or add tests; hidden tests will check your fix.
Python with Django's dependencies: $PY
Existing tests: cd tests && PYTHONPATH=.. $PY runtests.py --settings=test_sqlite --parallel 1 auth_tests"

say() { printf '\033[2m# %s\033[0m\n' "$*"; sleep 1.5; }
run() {
    printf '\033[1;32m$\033[0m '
    for ((i = 0; i < ${#1}; i++)); do printf '%s' "${1:i:1}"; sleep 0.02; done
    printf '\n'
    eval "$1"
    sleep 2
}

say 'Django ticket #30257: the username validators accept "alice\n".'
say 'Three Claude Code agents fix it at once, each in its own strict foil branch:'
run 'foil fanout -n 3 --name username --agent claude $SANDBOX --check "$CHECK" -- $CLAUDE -p "$TASK" 2>&1 | tee /tmp/fanout.txt'
best=$(sed -n 's/^best: //p' /tmp/fanout.txt)
say "Nothing has touched the checkout yet. Review the best fix:"
run "git status --short; foil diff --patch $best"
run "foil pick $best"
run 'git diff --stat'
say 'One branch landed; the other two are gone.'
