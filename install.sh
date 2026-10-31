#!/bin/sh
# Install foil into /usr/local/bin (Linux or macOS) and, on Ubuntu, allow it
# to use user namespaces.
#   ./install.sh                        from a checkout: build a static binary
#   curl -fsSL https://raw.githubusercontent.com/ai-agentic-tools/foil/main/install.sh | sh
#                                       download the latest release (SHA-256 checked)
#   PREFIX=$HOME/.local ./install.sh    no sudo for the copy (setup still needs it)
set -eu
PREFIX=${PREFIX:-/usr/local}
REPO=${FOIL_REPO:-ai-agentic-tools/foil}
TARGET=x86_64-unknown-linux-musl
OS=$(uname -s)
case $OS in Linux|Darwin) ;; *) echo "foil runs on Linux and macOS" >&2; exit 1 ;; esac

here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)
if [ -n "$here" ] && grep -qs '^name = "foil"' "$here/Cargo.toml" && [ -z "${FOIL_DOWNLOAD:-}" ]; then
    cd "$here"
    OUT=${CARGO_TARGET_DIR:-target}
    # A static binary needs musl-gcc (apt: musl-tools) for the TLS library;
    # without it, build a normal one.
    if [ "$OS" = Linux ] && [ "$(uname -m)" = x86_64 ] && command -v musl-gcc >/dev/null && rustup target add $TARGET >/dev/null 2>&1 \
        && cargo build --release --target $TARGET; then
        BIN=$OUT/$TARGET/release/foil
    else
        cargo build --release
        BIN=$OUT/release/foil
    fi
else
    case $OS-$(uname -m) in
    Linux-x86_64) asset=foil-x86_64-linux ;;
    Darwin-*) asset=foil-macos-universal ;;
    *) echo "no release binary for $OS $(uname -m) yet; build from a checkout: git clone https://github.com/$REPO" >&2; exit 1 ;;
    esac
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    url=https://github.com/$REPO/releases/latest/download/$asset
    echo "downloading $url"
    curl -fsSL -o "$tmp/$asset" "$url"
    curl -fsSL -o "$tmp/$asset.sha256" "$url.sha256"
    if command -v sha256sum >/dev/null; then
        (cd "$tmp" && sha256sum -c --quiet "$asset.sha256")
    else
        (cd "$tmp" && shasum -a 256 -c --quiet "$asset.sha256")
    fi
    BIN=$tmp/$asset
fi

SUDO=
[ -w "$PREFIX/bin" ] || [ -w "$PREFIX" ] || SUDO=sudo
$SUDO mkdir -p "$PREFIX/bin"
$SUDO install -m 0755 "$BIN" "$PREFIX/bin/foil"
echo "installed $PREFIX/bin/foil"

if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ]; then
    echo "this system restricts user namespaces; installing foil's AppArmor profile"
    sudo "$PREFIX/bin/foil" setup
fi
"$PREFIX/bin/foil" doctor || true
