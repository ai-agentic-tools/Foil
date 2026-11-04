#!/bin/sh
# OpenSandbox's create benchmark (create_bench.py) against two servers on the
# same machine, through the same SDK: OpenSandbox's own server with its docker
# runtime, and `foil serve` on a real checkout. Reports go to $OUT.
#   bench/opensandbox-method/run.sh            (Linux; needs docker, the
#   opensandbox + opensandbox-server packages in $VENV, and foil)
#   N=100 LEVELS="1 10 20 50" WORKSPACE=~/foil-django-demo/django
set -eu
here=$(cd "$(dirname "$0")" && pwd)
FOIL=${FOIL:-foil}
VENV=${VENV:-$HOME/osb-venv}
N=${N:-100}
LEVELS=${LEVELS:-1 10 20 50}
WORKSPACE=${WORKSPACE:-$HOME/foil-django-demo/django}
OUT=${OUT:-$here/results}
mkdir -p "$OUT"
tmp=$(mktemp -d)

cat >"$tmp/osb.toml" <<EOF
[server]
host = "127.0.0.1"
port = 18200
api_key = "bench"
[runtime]
type = "docker"
execd_image = "opensandbox/execd:v1.1.0"
[store]
type = "sqlite"
path = "$tmp/osb.db"
[docker]
network_mode = "bridge"
EOF
"$VENV/bin/opensandbox-server" --config "$tmp/osb.toml" >"$tmp/osb.log" 2>&1 &
osb=$!
export OPEN_SANDBOX_API_KEY=bench
"$FOIL" serve --listen 127.0.0.1:18300 -w "$WORKSPACE" >"$tmp/foil.log" 2>&1 &
serve=$!
trap 'kill $osb $serve 2>/dev/null; wait 2>/dev/null; rm -rf "$tmp"' EXIT
for port in 18200 18300; do
    for _ in $(seq 100); do curl -sf "http://127.0.0.1:$port/health" >/dev/null && break; sleep 0.1; done
done

for c in $LEVELS; do
    for backend in docker foil; do
        case $backend in
        docker) domain=127.0.0.1:18200 image=ubuntu:24.04 ;;
        foil) domain=127.0.0.1:18300 image=foil ;;
        esac
        DOMAIN=$domain API_KEY=bench IMAGE=$image LABEL=$backend N=$N CONCURRENCY=$c \
            REPORT_PATH="$OUT/$backend-c$c.json" "$VENV/bin/python" "$here/create_bench.py" || true
        # What a run leaves behind once every sandbox was killed.
        case $backend in
        docker) echo "  left behind: $(docker ps -aq --filter ancestor=$image | wc -l) containers" ;;
        foil) echo "  left behind: $("$FOIL" list 2>/dev/null | grep -c '^osb-') branches" ;;
        esac
    done
done
