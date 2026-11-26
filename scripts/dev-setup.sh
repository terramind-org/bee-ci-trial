#!/usr/bin/env bash
# One-time (idempotent) machine setup for running this repository's checks:
# Rust toolchain from rust-toolchain.toml, protoc and its well-known types, bun,
# python3, git, a C toolchain with pkg-config and OpenSSL headers, and the bun
# dependencies of apps/web and harness/agents. A second run changes nothing and
# prints versions.
# Docker and agent CLIs are deliberately not installed here.
set -euo pipefail

BUN_VERSION="1.4.2"          # matches apps/web/Dockerfile and .github/workflows/e2e.yml
PROTOC_VERSION="33.0"        # used only when no protoc is installable from the OS
# sha256 of the protoc release zips, verified before extraction.
PROTOC_SHA256_linux_x86_64="d99c011b799e9e412064244f0be417e5d76c9b6ace13a2ac735330fa7d57ad8f"
PROTOC_SHA256_linux_aarch_64="4b96bc91f8b54d829b8c3ca2207ff1ceb774843321e4fa5a68502faece584272"
PROTOC_SHA256_osx_x86_64="e4e50a703147a92d1a5a2d3a34c9e41717f67ade67d4be72b9a466eb8f22fe87"
PROTOC_SHA256_osx_aarch_64="3cf55dd47118bd2efda9cd26b74f8bbbfcf5beb1bf606bc56ad4c001b543f6d3"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OS="$(uname -s)"
ARCH="$(uname -m)"

log() { printf '==> %s\n' "$*"; }
die() { printf 'dev-setup: error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  if have sudo; then SUDO="sudo"; fi
fi
as_root() {
  if [ "$(id -u)" -eq 0 ]; then "$@"
  elif [ -n "$SUDO" ]; then "$SUDO" "$@"
  else die "need root to run: $*"; fi
}

APT_UPDATED=0
apt_install() {
  if [ "$APT_UPDATED" -eq 0 ]; then
    as_root env DEBIAN_FRONTEND=noninteractive apt-get update -qq
    APT_UPDATED=1
  fi
  as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$@"
}

# ---- OS packages ---------------------------------------------------------
ensure_os_packages() {
  case "$OS" in
    Linux)
      have apt-get || die "unsupported Linux: apt-get not found (Debian/Ubuntu expected)"
      local missing=()
      have git || missing+=(git)
      have curl || missing+=(curl)
      have python3 || missing+=(python3)
      have unzip || missing+=(unzip)
      have cc || missing+=(build-essential)
      have make || missing+=(build-essential)
      have pkg-config || missing+=(pkg-config)
      [ -e /usr/include/openssl/ssl.h ] || missing+=(libssl-dev)
      [ -e /etc/ssl/certs/ca-certificates.crt ] || missing+=(ca-certificates)
      if [ "${#missing[@]}" -gt 0 ]; then
        log "installing: ${missing[*]}"
        apt_install "${missing[@]}" || die "apt-get could not install: ${missing[*]}"
      fi
      ;;
    Darwin)
      have git || die "git missing: run 'xcode-select --install'"
      have cc || die "C compiler missing: run 'xcode-select --install'"
      have curl || die "curl missing"
      have unzip || die "unzip missing"
      have python3 || die "python3 missing: run 'xcode-select --install' or 'brew install python'"
      if ! have pkg-config; then
        have brew || die "pkg-config missing and Homebrew not found"
        log "installing pkg-config"
        brew install pkgconf || die "brew could not install pkgconf"
      fi
      ;;
    *) die "unsupported OS: $OS" ;;
  esac
}

# ---- Rust ----------------------------------------------------------------
ensure_rust() {
  export PATH="$HOME/.cargo/bin:$PATH"
  if ! have rustup; then
    log "installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$TMPDIR_SETUP/rustup-init.sh" \
      || die "could not download rustup-init"
    sh "$TMPDIR_SETUP/rustup-init.sh" -y --profile minimal --default-toolchain none >/dev/null \
      || die "rustup installation failed"
  fi
  # rustup reads rust-toolchain.toml (channel and components) from the repo.
  (cd "$ROOT" && rustup show active-toolchain >/dev/null 2>&1) || {
    log "installing the toolchain pinned in rust-toolchain.toml"
    (cd "$ROOT" && rustup toolchain install --profile minimal >/dev/null) \
      || die "could not install the Rust toolchain from rust-toolchain.toml"
  }
  (cd "$ROOT" && rustc --version >/dev/null) || die "rustc does not run"
  if [ -n "${GITHUB_PATH:-}" ]; then echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"; fi
}

# ---- protoc --------------------------------------------------------------
protoc_has_well_known_types() {
  cat > "$TMPDIR_SETUP/probe.proto" <<'EOF'
syntax = "proto3";
import "google/protobuf/timestamp.proto";
message SetupProbe { google.protobuf.Timestamp at = 1; }
EOF
  protoc -I "$TMPDIR_SETUP" --descriptor_set_out=/dev/null "$TMPDIR_SETUP/probe.proto" >/dev/null 2>&1
}

ensure_protoc() {
  have protoc && protoc_has_well_known_types && return 0
  if [ "$OS" = Linux ]; then
    log "installing protobuf compiler and well-known types"
    if apt_install protobuf-compiler libprotobuf-dev && have protoc && protoc_has_well_known_types; then return 0; fi
  elif have brew; then
    log "installing protobuf"
    if brew install protobuf && have protoc && protoc_has_well_known_types; then return 0; fi
  fi
  log "downloading protoc $PROTOC_VERSION"
  local plat sum
  case "$OS-$ARCH" in
    Linux-x86_64) plat=linux-x86_64 ;;
    Linux-aarch64|Linux-arm64) plat=linux-aarch_64 ;;
    Darwin-x86_64) plat=osx-x86_64 ;;
    Darwin-arm64) plat=osx-aarch_64 ;;
    *) die "no protoc download for $OS-$ARCH" ;;
  esac
  case "$plat" in
    linux-x86_64) sum="$PROTOC_SHA256_linux_x86_64" ;;
    linux-aarch_64) sum="$PROTOC_SHA256_linux_aarch_64" ;;
    osx-x86_64) sum="$PROTOC_SHA256_osx_x86_64" ;;
    *) sum="$PROTOC_SHA256_osx_aarch_64" ;;
  esac
  curl -sSfL -o "$TMPDIR_SETUP/protoc.zip" \
    "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-${plat}.zip" \
    || die "could not download protoc"
  local got
  if have sha256sum; then got="$(sha256sum "$TMPDIR_SETUP/protoc.zip" | cut -d' ' -f1)"
  else got="$(shasum -a 256 "$TMPDIR_SETUP/protoc.zip" | cut -d' ' -f1)"; fi
  [ "$got" = "$sum" ] || die "protoc checksum mismatch (expected $sum, got $got)"
  mkdir -p "$HOME/.local"
  unzip -qo "$TMPDIR_SETUP/protoc.zip" -d "$HOME/.local"
  export PATH="$HOME/.local/bin:$PATH"
  have protoc || die "protoc compiler is missing after installation"
  protoc_has_well_known_types || die "protoc well-known .proto files are missing (install libprotobuf-dev)"
}

# ---- bun -----------------------------------------------------------------
ensure_bun() {
  export BUN_INSTALL="${BUN_INSTALL:-$HOME/.bun}"
  export PATH="$BUN_INSTALL/bin:$PATH"
  if have bun && [ "$(bun --version)" = "$BUN_VERSION" ]; then return 0; fi
  log "installing bun $BUN_VERSION"
  curl -fsSL https://bun.sh/install -o "$TMPDIR_SETUP/bun-install.sh" || die "could not download the bun installer"
  bash "$TMPDIR_SETUP/bun-install.sh" "bun-v$BUN_VERSION" >/dev/null || die "bun $BUN_VERSION installation failed"
  [ "$(bun --version)" = "$BUN_VERSION" ] || die "bun is $(bun --version), expected $BUN_VERSION"
  if [ -n "${GITHUB_PATH:-}" ]; then echo "$BUN_INSTALL/bin" >> "$GITHUB_PATH"; fi
}

ensure_bun_deps() {
  local dir
  for dir in apps/web harness/agents; do
    # A frozen install is a quick no-op when node_modules already matches.
    log "bun install --frozen-lockfile in $dir"
    (cd "$ROOT/$dir" && bun install --frozen-lockfile >/dev/null) \
      || die "bun install --frozen-lockfile failed in $dir"
  done
}

TMPDIR_SETUP="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_SETUP"' EXIT

ensure_os_packages
ensure_rust
ensure_protoc
ensure_bun
ensure_bun_deps

log "versions"
(cd "$ROOT" && {
  git --version
  rustup --version 2>&1 | head -1
  cargo --version
  rustc --version
  protoc --version
  echo "bun $(bun --version)"
  python3 --version
  cc --version | head -1
  pkg-config --version | sed 's/^/pkg-config /'
  pkg-config --modversion openssl 2>/dev/null | sed 's/^/openssl /' || true
})
echo "Add to your shell if needed: export PATH=\"\$HOME/.cargo/bin:\$HOME/.bun/bin:\$HOME/.local/bin:\$PATH\""
