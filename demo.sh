#!/bin/sh
# A two-minute demo: a "rogue agent" works on a repo under foil. It does its
# job (edits code), and also tries to wreck things. Everything happens in a
# scratch folder; nothing outside it is touched.
#   ./demo.sh            (uses `foil` on PATH, or FOIL=/path/to/foil)
set -eu
FOIL=${FOIL:-foil}
DEMO=$(mktemp -d "${HOME}/.foil-demo.XXXXXX")
export FOIL_HOME="$DEMO/store"
trap 'chmod -R u+rwx "$DEMO" 2>/dev/null; rm -rf "$DEMO"' EXIT
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "A small repo, and a precious file elsewhere in your home folder"
mkdir -p "$DEMO/repo/src" && cd "$DEMO/repo"
printf 'def login(user):\n    return check(user)  # flaky\n' > src/auth.py
printf 'precious\n' > "$DEMO/precious.txt"
ls -R; cat "$DEMO/precious.txt"

cat > "$DEMO/agent.sh" <<EOF
echo "agent: fixing the bug"
printf 'def login(user):\n    return retry(check, user)\n' > src/auth.py
echo "agent: adding a test"
printf 'def test_login(): assert login("a")\n' > src/test_auth.py
echo "agent: 'cleaning up' your home folder"
rm -f "$DEMO/precious.txt" 2>&1 || true
echo "agent: phoning home"
(curl -sS -m 3 https://example.com >/dev/null && echo "agent: data sent!") 2>&1 || echo "agent: network unreachable"
echo "agent: leaving a daemon behind"
(setsid sh -c 'while :; do sleep 1; done' &)
echo "agent: eating memory"
x=\$(head -c 400000000 /dev/zero | tr '\\0' a)
echo "agent: this line never prints"
EOF

say "foil run -b fix-login --protect --no-network --memory 256M -- sh agent.sh"
"$FOIL" run -b fix-login --protect --no-network --memory 256M -- sh "$DEMO/agent.sh" || echo "(exit status $?)"

say "Your home folder is untouched"
cat "$DEMO/precious.txt"
say "No daemon survived the run"
pgrep -f 'while :; do sleep 1' || echo "none running"
say "The real repo is unchanged so far"
cat src/auth.py
say "Review what the agent did: foil diff --patch fix-login"
"$FOIL" diff --patch fix-login
say "Keep it: foil commit fix-login"
"$FOIL" commit fix-login
cat src/auth.py
ls src
