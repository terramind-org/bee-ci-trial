#!/bin/sh
# Install Bee from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/terramind-org/bee/main/scripts/install.sh | sh
#
# Environment:
#   BEE_VERSION        version to install, e.g. 0.1.0 or v0.1.0 (default: latest release)
#   BEE_INSTALL_DIR    where the binaries go (default: $HOME/.local/bin)
#   BEE_DOWNLOAD_BASE  release download base URL
#                          (default: https://github.com/terramind-org/bee/releases/download)
set -eu

REPO="terramind-org/bee"
BASE="${BEE_DOWNLOAD_BASE:-https://github.com/terramind-org/bee/releases/download}"
INSTALL_DIR="${BEE_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }

fetch() { # fetch <url> <dest>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1" || die "download failed: $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1" || die "download failed: $1"
  else
    die "need curl or wget"
  fi
}

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64) target="aarch64-unknown-linux-gnu" ;;
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  *)
    die "unsupported platform: $os $arch; build from source (https://github.com/terramind-org/bee/blob/main/docs/install.md)"
    ;;
esac

need tar
need uname

version="${BEE_VERSION:-}"
if [ -z "$version" ]; then
  tmp_meta="$(mktemp)"
  fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp_meta"
  version="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp_meta" | head -n 1)"
  rm -f "$tmp_meta"
  [ -n "$version" ] || die "could not determine the latest release; set BEE_VERSION"
fi
version="${version#v}"

name="bee-${version}-${target}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

say "Downloading $name.tar.gz"
fetch "$BASE/v${version}/$name.tar.gz" "$work/$name.tar.gz"
fetch "$BASE/v${version}/$name.tar.gz.sha256" "$work/$name.tar.gz.sha256"

expected="$(cut -d' ' -f1 "$work/$name.tar.gz.sha256" | head -n 1)"
[ -n "$expected" ] || die "empty checksum file for $name.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$work/$name.tar.gz" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$work/$name.tar.gz" | cut -d' ' -f1)"
else
  die "need sha256sum or shasum to verify the download"
fi
if [ "$expected" != "$actual" ]; then
  die "checksum mismatch for $name.tar.gz (expected $expected, got $actual); nothing was installed"
fi
say "Checksum ok ($actual)"

tar -xzf "$work/$name.tar.gz" -C "$work" || die "could not unpack $name.tar.gz"
mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
for b in bee bee-server bee-daemon bee-github; do
  [ -f "$work/$name/$b" ] || die "archive is missing $b"
  if ! cp "$work/$name/$b" "$INSTALL_DIR/$b.new" \
    || ! chmod 755 "$INSTALL_DIR/$b.new" \
    || ! mv -f "$INSTALL_DIR/$b.new" "$INSTALL_DIR/$b"; then
    die "could not install $b into $INSTALL_DIR"
  fi
  say "Installed $INSTALL_DIR/$b"
done

say "Bee $version installed for $target."
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "Add it to your PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
