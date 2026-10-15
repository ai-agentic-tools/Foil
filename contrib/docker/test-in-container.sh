#!/bin/sh
# Run foil's whole test suite inside a Docker container, with the settings
# foil needs there (see contrib/docker/README.md). Run from the repository
# root after `cargo test --no-run --all-targets`. On an Ubuntu host, run
# `sudo target/debug/foil setup` first: the host's AppArmor profile must
# cover the foil binary's path, which is the same inside the container.
set -eu
ROOT=$(pwd)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
docker build -q -t foil-test -f contrib/docker/Dockerfile.test contrib/docker >/dev/null
BINS=$(cargo test --no-run --all-targets 2>&1 | grep -o "($TARGET/debug/deps/[^)]*)" | tr -d '()' | grep -v overhead | tr '\n' ' ')
# Same paths inside as outside: the test binaries hold absolute paths.
# /tmp is a tmpfs: branch stores must not be on the container's overlayfs.
docker run --rm \
    --security-opt seccomp="$ROOT/contrib/docker/foil-seccomp.json" \
    --security-opt apparmor=unconfined \
    --security-opt systempaths=unconfined \
    --user "$(id -u):$(id -g)" --tmpfs /tmp:exec,mode=1777 -e HOME=/tmp -e FOIL_REQUIRE_BRANCH=1 \
    -v "$TARGET:$TARGET" -v "$ROOT:$ROOT" -w "$ROOT" \
    foil-test sh -c "set -e; for t in $BINS; do echo \"== \$(basename \$t)\"; \$t; done"
