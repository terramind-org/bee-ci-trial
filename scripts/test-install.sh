#!/usr/bin/env bash
# Exercise packaging and installation with shell stubs; this intentionally builds no Rust.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
server_pid=""
cleanup() {
  [ -z "$server_pid" ] || kill "$server_pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64) target="aarch64-unknown-linux-gnu" ;;
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  *) echo "test-install: unsupported test host" >&2; exit 1 ;;
esac

bin_dir="$tmp/bin"
mkdir -p "$bin_dir"
for binary in bee bee-server bee-daemon bee-github; do
  cat >"$bin_dir/$binary" <<EOF
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then
  echo "${binary} 0.1.0"
fi
EOF
  chmod +x "$bin_dir/$binary"
done

"$root/scripts/package-release.sh" "$target" 0.1.0 "$bin_dir"
archive="bee-0.1.0-$target.tar.gz"
release_root="$tmp/releases"
mkdir -p "$release_root/v0.1.0"
mv "$root/dist/$archive" "$root/dist/$archive.sha256" "$release_root/v0.1.0/"

port="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$release_root" >"$tmp/http.log" 2>&1 &
server_pid="$!"
for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:$port/" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
curl -fsS "http://127.0.0.1:$port/" >/dev/null

install_dir="$tmp/install"
BEE_DOWNLOAD_BASE="http://127.0.0.1:$port" BEE_VERSION=0.1.0 \
  BEE_INSTALL_DIR="$install_dir" "$root/scripts/install.sh"
for binary in bee bee-server bee-daemon bee-github; do
  [ -x "$install_dir/$binary" ]
done
[ "$("$install_dir/bee" --version)" = "bee 0.1.0" ]
echo "install success: four binaries installed"

printf 'corrupt' >"$release_root/v0.1.0/$archive"
bad_dir="$tmp/bad-install"
if BEE_DOWNLOAD_BASE="http://127.0.0.1:$port" BEE_VERSION=0.1.0 \
  BEE_INSTALL_DIR="$bad_dir" "$root/scripts/install.sh" >"$tmp/bad.out" 2>&1; then
  echo "test-install: corrupted archive unexpectedly installed" >&2
  exit 1
fi
grep -q 'checksum mismatch' "$tmp/bad.out"
[ ! -e "$bad_dir" ]
echo "checksum mismatch: no files installed"

fake_bin="$tmp/fake-bin"
mkdir -p "$fake_bin"
cat >"$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo Plan9 ;;
  -m) echo mips ;;
esac
EOF
chmod +x "$fake_bin/uname"
if PATH="$fake_bin:$PATH" BEE_VERSION=0.1.0 "$root/scripts/install.sh" >"$tmp/unsupported.out" 2>&1; then
  echo "test-install: unsupported platform unexpectedly installed" >&2
  exit 1
fi
grep -q 'unsupported platform: Plan9 mips' "$tmp/unsupported.out"
echo "unsupported platform: rejected clearly"
