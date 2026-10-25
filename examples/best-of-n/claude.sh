#!/bin/sh
# demo.sh with real agents: three Claude Code sessions fix the same failing
# test at once, each in a strict branch (no access to your home folder,
# network only to api.anthropic.com, API key added by foil's proxy so the
# agent never sees it). foil runs the tests in each branch and commits the
# smallest passing fix.
#   ANTHROPIC_API_KEY=... ./claude.sh      (needs `claude` on PATH; MODEL=haiku by default)
set -eu
FOIL=${FOIL:-foil}
: "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY (a workspace API key)}"
DEMO=$(mktemp -d "${HOME}/.foil-best-of-n.XXXXXX")
export FOIL_HOME="$DEMO/store"
trap 'chmod -R u+rwx "$DEMO" 2>/dev/null; rm -rf "$DEMO"' EXIT
here=$(cd "$(dirname "$0")" && pwd)

cp -R "$here/app" "$DEMO/repo" && cd "$DEMO/repo"
git init -q && git add -A && git -c user.name=demo -c user.email=demo@example.com commit -qm init

"$FOIL" fanout -n "${N:-3}" --name fix --agent claude --timeout 10m --pick \
    --check "python3 -B -m unittest -q" -- \
    --model "${MODEL:-haiku}" --permission-mode acceptEdits \
    --allowedTools Read Edit Write "Bash(python3:*)" \
    -p "The tests in test_slug.py fail. Fix slug.py so that they pass; do not change the tests."
printf '\nThe picked fix, now in the repo:\n'
git diff
python3 -B -m unittest -q
