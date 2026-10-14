#!/bin/sh
# Install foil: build a static binary from this checkout, put it in
# /usr/local/bin, and on Ubuntu allow it to use user namespaces.
#   ./install.sh            (asks for sudo for the install step)
#   PREFIX=$HOME/.local ./install.sh   (no sudo; see the note on AppArmor)
set -eu
PREFIX=${PREFIX:-/usr/local}
TARGET=x86_64-unknown-linux-musl
case $(uname -s) in Linux) ;; *) echo "foil runs on Linux only" >&2; exit 1 ;; esac

OUT=${CARGO_TARGET_DIR:-target}
if [ "$(uname -m)" = x86_64 ] && rustup target add $TARGET >/dev/null 2>&1; then
    cargo build --release --target $TARGET
    BIN=$OUT/$TARGET/release/foil
else
    cargo build --release
    BIN=$OUT/release/foil
fi

SUDO=
[ -w "$PREFIX/bin" ] || SUDO=sudo
$SUDO install -D -m 0755 "$BIN" "$PREFIX/bin/foil"
echo "installed $PREFIX/bin/foil"

if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ]; then
    echo "this system restricts user namespaces; installing foil's AppArmor profile"
    sudo "$PREFIX/bin/foil" setup
fi
"$PREFIX/bin/foil" doctor || true
