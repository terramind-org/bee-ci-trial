#!/usr/bin/env bash
# Package the release binaries into bee-<version>-<target>.tar.gz plus a .sha256 file.
# usage: scripts/package-release.sh <target-triple> <version> [bin-dir]
set -euo pipefail

if [ $# -lt 2 ] || [ $# -gt 3 ]; then
  echo "usage: $0 <target-triple> <version> [bin-dir]" >&2
  exit 2
fi
target="$1"
version="$2"
root="$(cd "$(dirname "$0")/.." && pwd)"

default_target_dir="${CARGO_TARGET_DIR:-$root/target}"
if [ $# -eq 3 ]; then
  bin_dir="$3"
elif [ -d "$default_target_dir/$target/release" ]; then
  bin_dir="$default_target_dir/$target/release"
else
  bin_dir="$default_target_dir/release"
fi
out="$root/dist"

name="bee-${version}-${target}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"

for b in bee bee-server bee-daemon bee-github; do
  if [ ! -x "$bin_dir/$b" ]; then
    echo "package-release: missing $bin_dir/$b; build the release binaries first" >&2
    exit 1
  fi
  cp "$bin_dir/$b" "$stage/$name/$b"
done
cp "$root/LICENSE" "$root/README.md" "$stage/$name/"
[ -f "$root/NOTICE" ] && cp "$root/NOTICE" "$stage/$name/"

# gzip -n leaves the file name and mtime out of the gzip header.
archive="$out/$name.tar.gz"
tar -C "$stage" -cf - "$name" | gzip -n -9 > "$archive"

if command -v sha256sum >/dev/null 2>&1; then
  hash="$(sha256sum "$archive" | cut -d' ' -f1)"
else
  hash="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
fi
printf '%s  %s\n' "$hash" "$name.tar.gz" > "$archive.sha256"

echo "package-release: ok"
echo "$archive"
