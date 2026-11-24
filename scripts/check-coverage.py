#!/usr/bin/env python3
"""Check docs/coverage/features.csv against docs/PRODUCT.md.

Fails when a row has no area, an unknown area, no source, no verification kind, an agent-scenario
row without scenarios, or a scenario id that PRODUCT.md does not define; or when ids repeat.
Feature-to-test mappings live in docs/coverage/tests/*.csv (columns feature_id,test), one file per
task, so parallel work never edits features.csv. Unknown feature ids in a shard are errors.
With --require-tests, also fails for every row that no shard maps to a test.
"""
import csv, re, sys, pathlib

root = pathlib.Path(__file__).resolve().parent.parent
product = (root / "docs/PRODUCT.md").read_text()
areas = set(re.findall(r"^### (4\.\d+) ", product, re.M))
scenarios = set(re.findall(r"`(AS-[A-Z]+-\d+)`", product))
kinds = {"agent-scenario", "api-conformance", "cli-contract", "compat-suite",
         "fault-test", "perf-harness", "model-test"}
rows = list(csv.DictReader((root / "docs/coverage/features.csv").open()))
errors, seen = [], set()
ids = {r["id"] for r in rows}
tests = {}
for shard in sorted((root / "docs/coverage/tests").glob("*.csv")):
    for t in csv.DictReader(shard.open()):
        fid, test = (t.get("feature_id") or "").strip(), (t.get("test") or "").strip()
        if fid not in ids:
            errors.append(f"{shard.name}: unknown feature id {fid!r}")
        elif not test:
            errors.append(f"{shard.name}: {fid} has an empty test")
        else:
            tests.setdefault(fid, []).append(test)
for r in rows:
    rid = r.get("id", "")
    if not rid or rid in seen:
        errors.append(f"{rid or '<no id>'}: missing or duplicate id")
    seen.add(rid)
    if r["area"] not in areas:
        errors.append(f"{rid}: unknown area {r['area']!r}")
    if not r["source"]:
        errors.append(f"{rid}: no source")
    if r["verification"] not in kinds:
        errors.append(f"{rid}: unknown verification {r['verification']!r}")
    ids = r["scenarios"].split()
    if r["verification"] == "agent-scenario" and not ids:
        errors.append(f"{rid}: agent-scenario row with no scenarios")
    for s in ids:
        if s not in scenarios:
            errors.append(f"{rid}: scenario {s} not defined in PRODUCT.md")
    if "--require-tests" in sys.argv and rid not in tests:
        errors.append(f"{rid}: no test")
missing_areas = areas - {r["area"] for r in rows}
for a in sorted(missing_areas):
    errors.append(f"area {a} has no coverage rows")
for e in errors:
    print("coverage:", e, file=sys.stderr)
print(f"coverage: {len(rows)} features, {len(areas)} areas, {len(scenarios)} agent scenarios, "
      f"{len(tests)} with tests, {len(errors)} problems")
sys.exit(1 if errors else 0)
