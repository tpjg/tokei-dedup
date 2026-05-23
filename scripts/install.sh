#!/bin/sh
# tokei-dedup installer — fetches a release tarball from GitHub, verifies its
# SHA256 if a checker is available, and drops `dupe` and `dupe-lsp` into
# $BIN_DIR (default: $HOME/.local/bin).
#
# Usage:
#   curl -fsSL https://github.com/tpjg/tokei-dedup/releases/latest/download/install.sh | sh
#
# Environment overrides:
#   VERSION   release tag to install (default: latest, e.g. v0.1.0)
#   BIN_DIR   install directory (default: $HOME/.local/bin)
#   REPO      owner/name on GitHub (default: tpjg/tokei-dedup)

set -eu

REPO="${REPO:-tpjg/tokei-dedup}"
VERSION="${VERSION:-latest}"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

err() { printf 'install.sh: %s\n' "$*" >&2; }
die() { err "$*"; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"
command -v uname >/dev/null 2>&1 || die "uname is required"

uname_s=$(uname -s)
uname_m=$(uname -m)

case "$uname_s" in
  Linux)  os=linux ;;
  Darwin) os=darwin ;;
  *) die "unsupported OS: $uname_s" ;;
esac

case "$uname_m" in
  x86_64|amd64)   arch=x86_64 ;;
  arm64|aarch64)  arch=aarch64 ;;
  *) die "unsupported architecture: $uname_m" ;;
esac

case "$os-$arch" in
  linux-x86_64)
    if ldd --version 2>&1 | grep -qi musl; then
      target="x86_64-unknown-linux-musl"
    else
      target="x86_64-unknown-linux-gnu"
    fi
    ;;
  linux-aarch64)  target="aarch64-unknown-linux-gnu" ;;
  darwin-x86_64)  target="x86_64-apple-darwin" ;;
  darwin-aarch64) target="aarch64-apple-darwin" ;;
  *) die "unsupported platform: $os-$arch" ;;
esac

if [ "$VERSION" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

tarball="tokei-dedup-${target}.tar.gz"
url="$base/$tarball"
sum_url="$url.sha256"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t tokei-dedup)
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'Downloading %s\n' "$url"
curl -fL --proto '=https' --tlsv1.2 -o "$tmp/$tarball" "$url"

if curl -fLs --proto '=https' --tlsv1.2 -o "$tmp/$tarball.sha256" "$sum_url"; then
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$tarball.sha256")
  elif command -v shasum >/dev/null 2>&1; then
    expected=$(awk '{print $1}' "$tmp/$tarball.sha256")
    actual=$(shasum -a 256 "$tmp/$tarball" | awk '{print $1}')
    if [ "$expected" != "$actual" ]; then
      die "checksum mismatch: expected $expected, got $actual"
    fi
    printf 'Checksum OK\n'
  else
    err "warning: no sha256sum/shasum found; skipping checksum verification"
  fi
else
  err "warning: no checksum at $sum_url; skipping verification"
fi

mkdir -p "$BIN_DIR"
tar -C "$BIN_DIR" -xzf "$tmp/$tarball" dupe dupe-lsp
chmod +x "$BIN_DIR/dupe" "$BIN_DIR/dupe-lsp"

printf 'Installed dupe and dupe-lsp to %s\n' "$BIN_DIR"

case ":${PATH:-}:" in
  *":$BIN_DIR:"*) ;;
  *)
    printf '\nNote: %s is not on your PATH. Add it with:\n' "$BIN_DIR"
    printf '  export PATH="%s:$PATH"\n' "$BIN_DIR"
    ;;
esac
