#!/bin/sh
# Best of N, safely: four "agents" attack one failing test at the same time,
# each in its own copy-on-write branch of the repo. foil runs the tests in
# every branch, keeps the smallest passing fix, and throws the rest away.
# One attempt goes rogue (reads secrets, plants a backdoor, phones home,
# then hangs); it gets nothing and is stopped by --timeout.
#
# No API key needed: the "agents" are scripted, so the demo is the same
# every time. claude.sh next to this file does the same with Claude Code.
#   ./demo.sh            (uses `foil` on PATH, or FOIL=/path/to/foil)
set -eu
FOIL=${FOIL:-foil}
DEMO=$(mktemp -d "${HOME}/.foil-best-of-n.XXXXXX")
export FOIL_HOME="$DEMO/store"
trap 'chmod -R u+rwx "$DEMO" 2>/dev/null; rm -rf "$DEMO"' EXIT
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
here=$(cd "$(dirname "$0")" && pwd)

say "A repo with a failing test"
cp -R "$here/app" "$DEMO/repo" && cd "$DEMO/repo"
git init -q && git add -A && git -c user.name=demo -c user.email=demo@example.com commit -qm init
python3 -B -m unittest 2>&1 | tail -3 || true

# Things a rogue agent would love: a secret and a shell profile outside the repo.
mkdir -p "$DEMO/home"
echo "sk-live-do-not-leak" > "$DEMO/secret.txt"
echo "# my shell profile" > "$DEMO/home/.bashrc"
sed "s|@DEMO@|$DEMO|g" "$here/agent.sh" > "$DEMO/agent.sh"

say "foil fanout -n 4 --strict --timeout 5s --check 'python3 -B -m unittest' -- sh agent.sh"
started=$(date +%s)
"$FOIL" fanout -n 4 --name fix --strict --timeout 5s --allow-read "$DEMO/agent.sh" \
    --check "python3 -B -m unittest -q" -- sh "$DEMO/agent.sh" || true
echo "(4 attempts, tests in each, $(($(date +%s) - started)) s)"

say "What the rogue attempt tried, and got"
# (The log goes on with the check's test output; the run's own part ends at foil's note.)
sed '/^foil:/q' "$FOIL_HOME/fanout/fix/attempt-4.log"

say "Nothing reached the repo yet; the winner is a reviewable diff"
git status --short; echo "(clean)"
"$FOIL" diff --patch fix-3

say "foil pick fix-3: commit it, discard the other three"
"$FOIL" pick fix-3
git status --short
python3 -B -m unittest -q 2>&1 | tail -1

say "Outside the repo, nothing changed"
cat "$DEMO/secret.txt" "$DEMO/home/.bashrc"
"$FOIL" list; echo "(no branches left)"
