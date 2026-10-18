#!/bin/sh
# Platform matrix (family M of the quality plan): run foil's whole test suite
# on several Linux distributions and kernels, each in its own Lima VM.
#
#   bench/matrix/lima-matrix.sh [template ...]
#
# Default templates: ubuntu-22.04 (kernel 5.15), debian-12 (6.1),
# fedora (latest, SELinux enforcing), almalinux-9 (RHEL 9 kernel 5.14).
# Each VM is created on first use, gets Rust and the test tools, builds foil
# from this checkout (mounted read-only) into a VM-local target folder, runs
# `foil doctor` and the suite, and is stopped afterwards. Results go to
# bench/matrix/results/<template>.txt; a summary is printed at the end.
set -u
cd "$(dirname "$0")/../.."
REPO=$(pwd)
OUT=$REPO/bench/matrix/results
mkdir -p "$OUT"
TEMPLATES=${*:-ubuntu-22.04 debian-12 fedora almalinux-9}

for t in $TEMPLATES; do
    vm=foil-$(echo "$t" | tr . -)
    log=$OUT/$t.txt
    echo "== $t ($vm)"
    if ! limactl list -q 2>/dev/null | grep -qx "$vm"; then
        limactl create --tty=false --name "$vm" --cpus 2 --memory 3 "template://$t" >/dev/null 2>&1 || { echo "create failed" >"$log"; continue; }
    fi
    limactl start --tty=false "$vm" >/dev/null 2>&1 || { echo "start failed" >"$log"; continue; }
    limactl shell --workdir / "$vm" sh -s >"$log" 2>&1 <<EOF
set -u
echo "## system"; uname -r; . /etc/os-release; echo "\$PRETTY_NAME"
if command -v apt-get >/dev/null; then
    sudo apt-get update -qq >/dev/null && sudo apt-get install -y -qq build-essential curl git python3 procps diffutils >/dev/null
else
    sudo dnf install -y -q gcc make curl git python3 procps-ng diffutils policycoreutils >/dev/null 2>&1
fi
command -v getenforce >/dev/null && echo "SELinux: \$(getenforce)"
[ -x \$HOME/.cargo/bin/cargo ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null 2>&1
export PATH=\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR=\$HOME/target
cd $REPO
echo "## build"; cargo build -q 2>&1 | tail -5
F=\$CARGO_TARGET_DIR/debug/foil
echo "## setup"; sudo \$F setup 2>&1 | tail -2
echo "## doctor"; \$F doctor 2>&1
echo "## tests (FOIL_REQUIRE_BRANCH=1)"
FOIL_REQUIRE_BRANCH=1 FOIL_REQUIRE_SUDO=1 FOIL_DIFF_CASES=200 cargo test --no-fail-fast 2>&1 | grep -E "^test result|FAILED|panicked|^---- " | head -60
echo "## done"
EOF
    limactl stop "$vm" >/dev/null 2>&1
    passed=$(grep "^test result" "$log" | awk '{n+=$4} END {print n+0}')
    failed=$(grep "^test result" "$log" | awk '{n+=$6} END {print n+0}')
    echo "   kernel $(sed -n 2p "$log")  passed $passed  failed $failed"
done
