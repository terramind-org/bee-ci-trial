#!/usr/bin/env python3
"""Fail if any workspace crate depends on a crate in the same or a higher layer."""
import json, subprocess, sys, pathlib

root = pathlib.Path(__file__).resolve().parent.parent
# layers.toml is flat `name = number` lines; parsed by hand so the check runs on any Python 3.
layers = {}
for line in (root / "scripts/layers.toml").read_text().splitlines():
    line = line.split("#", 1)[0].strip()
    if "=" in line:
        name, value = (part.strip() for part in line.split("=", 1))
        layers[name] = int(value)
meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root))
bad = []
for pkg in meta["packages"]:
    name = pkg["name"]
    if name not in layers:
        bad.append(f"{name}: not listed in scripts/layers.toml")
        continue
    for dep in pkg["dependencies"]:
        d = dep["name"]
        if d.startswith("bee-") and dep.get("kind") != "dev":
            if d not in layers:
                bad.append(f"{name} -> {d}: {d} not listed")
            elif layers[d] >= layers[name]:
                bad.append(f"{name} (layer {layers[name]}) -> {d} (layer {layers[d]})")
for b in bad:
    print("layer violation:", b, file=sys.stderr)
sys.exit(1 if bad else 0)
