#!/bin/sh
# One-time setup for run.sh: Django at the commit just before ticket #30257
# was fixed, and a Python 3.9 environment with its dependencies (via uv).
#   ./setup.sh             (WORK=~/foil-django-demo by default)
set -eu
WORK=${WORK:-$HOME/foil-django-demo}
here=$(cd "$(dirname "$0")" && pwd)
BASE=d26b2424437dabeeca94d7900b37d2df4410da0c
command -v uv >/dev/null || { echo "needs uv: curl -LsSf https://astral.sh/uv/install.sh | sh" >&2; exit 1; }
mkdir -p "$WORK"

if [ ! -d "$WORK/django/.git" ]; then
    echo "fetching Django at ${BASE%"${BASE#???????}"} (one commit, ~40 MB)"
    git init -q "$WORK/django"
    git -C "$WORK/django" fetch -q --depth 1 https://github.com/django/django.git "$BASE"
    git -C "$WORK/django" checkout -q FETCH_HEAD
fi
if [ ! -x "$WORK/env/bin/python" ]; then
    echo "creating a Python 3.9 environment"
    uv venv -q --python 3.9 "$WORK/env"
    uv pip install -q --python "$WORK/env/bin/python" asgiref sqlparse pytz
fi

echo "the bug, before any fix:"
cd "$WORK/django"
"$WORK/env/bin/python" -B "$here/check.py" || true
