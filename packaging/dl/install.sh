#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Hyphae installer — https://github.com/Hyphae-Research-Foundation/hyphae
#
# Downloads the official release binary for this platform, verifies its
# SHA-256 against the release's signed SHA256SUMS, and installs it to
# ~/.local/bin (override with HYPHAE_BIN_DIR). Pin a version with
# HYPHAE_VERSION=x.y.z. Nothing runs with sudo and nothing else is
# touched; Agent Memory setup stays an explicit second step.
set -eu

REPO="Hyphae-Research-Foundation/hyphae"
BIN_DIR="${HYPHAE_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"

if [ -z "${HYPHAE_FORCE:-}" ] && command -v pacman >/dev/null 2>&1; then
  say "Arch/Omarchy detected — prefer the AUR package so pacman owns the binary:"
  say "  omarchy pkg aur add hyphae-bin    # or: paru -S hyphae-bin"
  say "Re-run with HYPHAE_FORCE=1 to install to $BIN_DIR anyway."
  exit 0
fi

os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64)  target="x86_64-unknown-linux-gnu" ;;
  Darwin/arm64)  target="aarch64-apple-darwin" ;;
  Darwin/x86_64) target="x86_64-apple-darwin" ;;
  *) fail "unsupported platform $os/$arch — releases: https://github.com/$REPO/releases" ;;
esac

version="${HYPHAE_VERSION:-}"
if [ -z "$version" ]; then
  version=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name" *: *"v\([^"]*\)".*/\1/p' | head -n1)
fi
[ -n "$version" ] || fail "could not resolve the latest version"

asset="hyphae-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/v$version"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

say "Downloading $asset ..."
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected=$(awk -v name="$asset" '$2 == name {print $1}' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "$asset is not covered by SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] || fail "SHA-256 mismatch for $asset"
say "Verified SHA-256 $actual"

tar -xzf "$tmp/$asset" -C "$tmp"
found=$(find "$tmp" -type f -name hyphae | head -n1)
[ -n "$found" ] || fail "the archive does not contain the hyphae binary"
mkdir -p "$BIN_DIR"
install -m 0755 "$found" "$BIN_DIR/hyphae"

say "Installed hyphae $version to $BIN_DIR/hyphae"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "Note: add $BIN_DIR to your PATH." ;;
esac
say ""
say "Next — local, shared, verifiable memory for your coding agents:"
say "  hyphae agent setup"
