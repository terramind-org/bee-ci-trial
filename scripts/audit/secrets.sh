#!/usr/bin/env bash
# Scan every reachable Git ref without leaving a report in the worktree.
set -euo pipefail

readonly GITLEAKS_VERSION="8.28.0"
readonly GITLEAKS_SHA256="a65b5253807a68ac0cafa4414031fd740aeb55f54fb7e55f386acb52e6a840eb"
readonly GITLEAKS_URL="https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz"

repo_root="$(git rev-parse --show-toplevel)"
report_path="${BEE_SECRETS_REPORT:-$(mktemp "${TMPDIR:-/tmp}/bee-gitleaks.XXXXXX.json")}"
if [[ "$report_path" == "$repo_root"/* ]]; then
  echo "refusing to write a secret scan report inside the repository" >&2
  exit 2
fi

if command -v gitleaks >/dev/null 2>&1; then
  gitleaks_bin="$(command -v gitleaks)"
else
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64) ;;
    *) echo "gitleaks fallback supports Linux x86_64 only; install gitleaks" >&2; exit 2 ;;
  esac
  download_dir="$(mktemp -d "${TMPDIR:-/tmp}/bee-gitleaks.XXXXXX")"
  trap 'rm -rf "$download_dir"' EXIT
  archive="$download_dir/gitleaks.tar.gz"
  curl --fail --location --silent --show-error "$GITLEAKS_URL" --output "$archive"
  printf '%s  %s\n' "$GITLEAKS_SHA256" "$archive" | sha256sum --check --status
  tar -xzf "$archive" -C "$download_dir"
  gitleaks_bin="$download_dir/gitleaks"
fi

"$gitleaks_bin" git --redact --config "$repo_root/.gitleaks.toml" \
  --report-format json --report-path "$report_path" "$repo_root"
printf 'gitleaks report: %s\n' "$report_path"
