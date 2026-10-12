#!/bin/sh
# zenshim installer — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/rabden/zenshim/main/install.sh | sh
# Or after checkout: sh install.sh [--dest <dir>]

set -eu

REPO="rabden/zenshim"

say() { printf '%s\n' "$*"; }

# --- destination -----------------------------------------------------------
DEST=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dest) DEST="$2"; shift 2 ;;
    *) say "unknown option: $1"; exit 1 ;;
  esac
done

if [ -z "$DEST" ]; then
  if [ -w /usr/local/bin ] 2>/dev/null; then
    DEST="/usr/local/bin"
  else
    DEST="${HOME}/.local/bin"
    mkdir -p "$DEST"
  fi
fi

# --- platform detection ----------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS:$ARCH" in
  Linux:x86_64)          TARGET="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  Darwin:x86_64)         TARGET="x86_64-apple-darwin" ;;
  Darwin:arm64)          TARGET="aarch64-apple-darwin" ;;
  *)
    say "unsupported platform: $OS $ARCH"
    say "download a binary manually from https://github.com/$REPO/releases"
    exit 1
    ;;
esac

# --- resolve latest release -------------------------------------------------
TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/')"
if [ -z "$TAG" ]; then
  say "could not resolve the latest release"
  exit 1
fi

ASSET="zenshim-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

say "installing zenshim $TAG ($TARGET) -> $DEST"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if ! curl -fsSL "$URL" -o "$TMP/$ASSET"; then
  say "download failed: $URL"
  say "no prebuilt binary for this target yet? check https://github.com/$REPO/actions"
  exit 1
fi

tar xzf "$TMP/$ASSET" -C "$TMP"
install -m 0755 "$TMP/zenshim" "$DEST/zenshim"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) say "note: $DEST is not on your PATH — add it to run 'zenshim' from anywhere" ;;
esac

"$DEST/zenshim" --version
say "done. run: zenshim"
