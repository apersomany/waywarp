#!/bin/sh
# Installs the latest Waywarp release, or WAYWARP_VERSION (such as v0.1.0), into WAYWARP_PREFIX/bin.
# Usage: curl -fsSL https://raw.githubusercontent.com/apersomany/waywarp/master/install.sh | sh
set -eu

repository=apersomany/waywarp
prefix=${WAYWARP_PREFIX:-/usr/local}
version=${WAYWARP_VERSION:-latest}

fail() {
    echo "waywarp install: $*" >&2
    exit 1
}

[ "$(uname -s)" = Linux ] || fail "Waywarp runs only on Linux"
case $(uname -m) in
    x86_64 | amd64) system=x86_64-linux ;;
    aarch64 | arm64) system=aarch64-linux ;;
    *) fail "no release for $(uname -m); build from source with cargo instead" ;;
esac

if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -qO "$2" "$1"; }
else
    fail "needs curl or wget"
fi

if [ "$version" = latest ]; then
    base=https://github.com/$repository/releases/latest/download
else
    base=https://github.com/$repository/releases/download/$version
fi

directory=$(mktemp -d)
trap 'rm -rf "$directory"' EXIT INT TERM
cd "$directory"

echo "Downloading waywarp-$system ($version)"
download "$base/waywarp-$system" "waywarp-$system" || fail "cannot download waywarp-$system"
download "$base/SHA256SUMS" SHA256SUMS || fail "cannot download SHA256SUMS"
grep " waywarp-$system\$" SHA256SUMS | sha256sum -c - >/dev/null || fail "checksum mismatch"

# Provenance proves the binary came from this repository's release workflow.
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh attestation verify "waywarp-$system" --repo "$repository" >/dev/null ||
        fail "provenance verification failed"
    echo "Verified build provenance"
fi

# Writing to the default prefix usually needs root.
target=$prefix/bin/waywarp
run=
if ! mkdir -p "$prefix/bin" 2>/dev/null || ! [ -w "$prefix/bin" ]; then
    command -v sudo >/dev/null 2>&1 || fail "cannot write to $prefix/bin; set WAYWARP_PREFIX"
    run=sudo
fi
$run install -D -m 755 "waywarp-$system" "$target"
echo "Installed $("$target" --version) to $target"

for tool in warp-svc warp-cli ip nft; do
    command -v "$tool" >/dev/null 2>&1 ||
        echo "Note: $tool is not on PATH; Waywarp needs Cloudflare WARP, iproute2, and nftables" >&2
done
