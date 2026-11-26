#!/usr/bin/env python3
"""Audit third-party licence metadata used to distribute Bee binaries."""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUSTUP_BIN = Path.home() / ".cargo" / "bin"
if RUSTUP_BIN.is_dir():
    os.environ["PATH"] = f"{RUSTUP_BIN}{os.pathsep}{os.environ.get('PATH', '')}"

# A licence expression with at least one of these alternatives is distributable
# with Apache-2.0 binaries. Add exceptions here only with a legal rationale.
COMPATIBLE = {
    "Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib",
    "Unicode-3.0", "CC0-1.0", "Unlicense", "MPL-2.0", "MIT-0", "0BSD",
    "OFL-1.1", "CDLA-Permissive-2.0", "Python-2.0", "CC-BY-4.0",
    "BlueOak-1.0.0",
}
# These prebuilt sharp packages contain dynamically loaded libvips shared
# libraries. They are not statically linked into the Apache-2.0 web image.
ALLOWLIST = {
    "apps/web:@img/sharp-libvips-linux-x64@1.3.4": "LGPL library is dynamically loaded",
    "apps/web:@img/sharp-libvips-linuxmusl-x64@1.3.4": "LGPL library is dynamically loaded",
}
INCOMPATIBLE_MARKERS = ("GPL", "AGPL", "LGPL", "SSPL", "BUSL")


def command(*args: str, cwd: Path = ROOT) -> str:
    return subprocess.check_output(args, cwd=cwd, text=True)


def compatible(license_name: str | None) -> bool:
    if not license_name:
        return False
    normalized = license_name.replace("(", "").replace(")", "")
    alternatives = normalized.replace("/", " OR ").split(" OR ")
    return any(
        all(part.strip() in COMPATIBLE for part in alternative.split(" AND "))
        for alternative in alternatives
    )


def emit(source: str, name: str, version: str, license_name: str | None) -> bool:
    display = license_name or "MISSING"
    key = f"{source}:{name}@{version}"
    flagged = not compatible(license_name)
    if flagged and key in ALLOWLIST:
        print(f"ALLOWLISTED {source} {name}@{version}: {display} ({ALLOWLIST[key]})")
        return False
    print(f"{('FLAG' if flagged else 'OK')} {source} {name}@{version}: {display}")
    return flagged


def rust_dependencies() -> list[tuple[str, str, str | None]]:
    metadata = json.loads(command("cargo", "metadata", "--format-version", "1", "--locked"))
    packages = metadata["packages"]
    workspace = set(metadata["workspace_members"])
    return sorted(
        (pkg["name"], pkg["version"], pkg.get("license"))
        for pkg in packages if pkg["id"] not in workspace
    )


def npm_dependencies(directory: Path) -> list[tuple[str, str, str | None]]:
    node_modules = directory / "node_modules"
    if not node_modules.is_dir():
        raise RuntimeError(f"{node_modules} is missing; run bun install --frozen-lockfile first")
    result: list[tuple[str, str, str | None]] = []
    for manifest in node_modules.glob("**/package.json"):
        if "/node_modules/" not in str(manifest):
            continue
        try:
            package = json.loads(manifest.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            raise RuntimeError(f"cannot read {manifest}: {exc}") from exc
        name, version = package.get("name"), package.get("version")
        if name and version:
            result.append((name, version, package.get("license")))
    return sorted(set(result))


def main() -> int:
    flagged = False
    for name, version, license_name in rust_dependencies():
        flagged |= emit("cargo", name, version, license_name)
    for relative in (Path("apps/web"), Path("harness/agents")):
        for name, version, license_name in npm_dependencies(ROOT / relative):
            flagged |= emit(str(relative), name, version, license_name)
    return int(flagged)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileNotFoundError, RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"license audit failed: {exc}", file=sys.stderr)
        raise SystemExit(2)
