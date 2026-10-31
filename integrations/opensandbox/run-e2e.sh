#!/bin/sh
# Run sdk_e2e.py against a fresh `foil serve` (needs the opensandbox SDK in PYTHON).
#   integrations/opensandbox/run-e2e.sh [foil serve run options, e.g. --strict]
set -eu
FOIL=${FOIL:-foil}
PYTHON=${PYTHON:-python3}
PORT=${PORT:-18091}
here=$(cd "$(dirname "$0")" && pwd)
ws=$(mktemp -d)
log=$(mktemp)
export OPEN_SANDBOX_API_KEY=e2e-$$ OPEN_SANDBOX_DOMAIN=127.0.0.1:$PORT OPENSANDBOX_DISABLE_METRICS=1
"$FOIL" serve --listen "127.0.0.1:$PORT" -w "$ws" "$@" >"$log" 2>&1 &
serve=$!
trap 'kill $serve 2>/dev/null; cat "$log"' EXIT
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null && break; sleep 0.1; done
"$PYTHON" "$here/sdk_e2e.py" "$ws"
