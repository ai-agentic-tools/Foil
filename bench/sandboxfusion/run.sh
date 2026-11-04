#!/bin/bash
# HumanEval's canonical solutions through SandboxFusion in three modes on one
# machine: none (no sandbox), lite (SandboxFusion's own, as root) and foil (as
# you). Then the leftovers each mode leaves, after a normal run and after the
# server is killed mid-run.
#   SF=~/showcase/SandboxFusion PY=~/sf-conda/venv/bin/python DATA=~/showcase/HumanEval.jsonl \
#     bench/sandboxfusion/run.sh
set -u
here=$(cd "$(dirname "$0")" && pwd)
SF=${SF:-$HOME/showcase/SandboxFusion}
PY=${PY:-$HOME/sf-conda/venv/bin/python}
DATA=${DATA:-$HOME/showcase/HumanEval.jsonl}
LEVELS=${LEVELS:-1 8 32}
MODES=${MODES:-none lite foil}
OUT=${OUT:-$here/results}
PORT=18400
mkdir -p "$OUT" "$SF/docs/build"   # the server serves its docs from here
[ -f "$SF/docs/build/index.html" ] || echo '<p>docs not built</p>' >"$SF/docs/build/index.html"
export FOIL_ALLOW_READ="$HOME/sf-conda:$(uv python dir)"

start() {  # mode
    cd "$SF"
    if [ "$1" = lite ]; then
        sudo mkdir -p /var/tmp/sf
        sudo env PATH="$PATH" SANDBOX_CONFIG=bench_lite SANDBOX_TMP_DIR=/var/tmp/sf "$PY" -m uvicorn sandbox.server.server:app --port $PORT >"$OUT/server-$1.log" 2>&1 &
    else
        SANDBOX_CONFIG=bench_$1 "$PY" -m uvicorn sandbox.server.server:app --port $PORT >"$OUT/server-$1.log" 2>&1 &
    fi
    cd - >/dev/null
    for _ in $(seq 100); do curl -sf -o /dev/null "http://127.0.0.1:$PORT/v1/ping" && return; sleep 0.2; done
    echo "server ($1) didn't start:"; tail -5 "$OUT/server-$1.log"; return 1
}
stop() {
    for pid in $(pgrep -f "uvicorn sandbox.server.server:app --port $PORT"); do sudo kill -"${1:-TERM}" "$pid" 2>/dev/null; done
    sleep 2
}
leftovers() {  # what is left on the machine
    echo "    leftover processes: $(pgrep -fc 'sleep 314.159')  overlay mounts: $(grep -c ' /tmp/overlay_' /proc/mounts)" \
         " netns: $(ip netns list 2>/dev/null | wc -l)  cgroups: $(find /sys/fs/cgroup -maxdepth 2 -name 'sandbox_*' 2>/dev/null | wc -l)"
}
clean() {  # each mode starts on a clean machine, so its leftovers are its own
    sudo pkill -f 'sleep 314.159'
    for _ in 1 2 3 4; do grep ' /tmp/overlay_' /proc/mounts | awk '{print $2}' | sort -r | xargs -r sudo umount -l 2>/dev/null; done
    ip netns list 2>/dev/null | awk '{print $1}' | while read -r n; do sudo ip netns delete "$n"; done
    find /sys/fs/cgroup -maxdepth 2 -name 'sandbox_*' 2>/dev/null | while read -r c; do sudo rmdir "$c"; done
}

for mode in $MODES; do
    clean
    start "$mode" || { stop; continue; }
    for c in $LEVELS; do
        "$PY" "$here/drive.py" "http://127.0.0.1:$PORT" "$DATA" "$c" "$mode" "$OUT/$mode-c$c.json"
    done
    leftovers
    # Reliability: kill the server (SIGKILL) while 32 programs are running.
    "$PY" "$here/drive.py" "http://127.0.0.1:$PORT" "$DATA" 32 "$mode-killed" >/dev/null 2>&1 &
    sleep 1.5
    stop KILL
    wait 2>/dev/null
    echo "  after kill -9 of the server mid-run:"
    sleep 3
    leftovers
    stop
done
clean
