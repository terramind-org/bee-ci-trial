#!/usr/bin/env python3
"""Unify exact (`=x.y.z`) dependency pins across workspace crates to the highest version in use.

Crates pin versions in their own Cargo.toml (so parallel work never edits the root manifest), and
two crates pinning different exact versions of one dependency make the workspace unresolvable.
Run after merging branches; `--check` exits 1 if any pin differs (used by scripts/check.sh).
"""
import pathlib, re, sys

root = pathlib.Path(__file__).resolve().parent.parent
manifests = sorted(root.glob("crates/*/Cargo.toml")) + sorted(root.glob("bench/*/Cargo.toml"))
pin = re.compile(r'^(\s*([A-Za-z0-9_-]+)\s*=\s*(?:\{[^}\n]*?version\s*=\s*)?")=(\d+(?:\.\d+)*(?:-[0-9A-Za-z.]+)?)(")', re.M)

def key(v):
    return tuple(int(p) if p.isdigit() else -1 for p in re.split(r"[.-]", v))

best = {}
for m in manifests:
    if m.parts[-3] == "bench":
        continue  # standalone crates resolve on their own
    for g in pin.finditer(m.read_text()):
        name, ver = g.group(2), g.group(3)
        if name not in best or key(ver) > key(best[name]):
            best[name] = ver

changed = []
for m in manifests:
    if m.parts[-3] == "bench":
        continue
    text = m.read_text()
    new = pin.sub(lambda g: f'{g.group(1)}={best[g.group(2)]}{g.group(4)}', text)
    if new != text:
        changed.append(m.relative_to(root))
        if "--check" not in sys.argv:
            m.write_text(new)
for c in changed:
    print(("differs: " if "--check" in sys.argv else "updated: ") + str(c))
sys.exit(1 if changed and "--check" in sys.argv else 0)
