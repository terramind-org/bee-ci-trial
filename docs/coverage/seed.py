#!/usr/bin/env python3
"""One-off seeder: turns the three source audits into docs/coverage/features.csv.

After seeding, features.csv is maintained by hand; rerunning this overwrites it.
Every row: id, area (PRODUCT.md §4.x), feature, source, v02_status, verification, scenarios.
"""
import csv, re, pathlib
root = pathlib.Path(__file__).resolve().parents[2]
R = root / "docs/research"

# Agent scenarios per area, as defined in docs/PRODUCT.md.
SCEN = {}
for m in re.finditer(r"### (4\.\d+) .*?(?=\n### |\n## |\Z)", (root/"docs/PRODUCT.md").read_text(), re.S):
    SCEN[m.group(1)] = re.findall(r"`(AS-[A-Z]+-\d+)`", m.group(0))

VERIF = {"4.23": "perf-harness", "4.22": "fault-test", "4.24": "api-conformance",
         "4.25": "model-test", "4.7": "cli-contract", "4.8": "api-conformance",
         "4.11": "compat-suite", "4.12": "compat-suite"}

def area_kw(text, default):
    t = text.lower()
    rules = [
        (("audit log",), "4.10"), (("redact", "quarantin", "purge", "takedown"), "4.10"),
        (("superfan",), "4.19"), (("backup",), "4.3"),
        (("git gateway", "smart http", "ssh git", "lfs", "submodule", "source archive", "protocol v2", "packfile"), "4.11"),
        (("ruleset", "branch protection", "signed commit", "push rule", "tag protection", "role", "token", "oauth", "saml", "scim", "2fa", "passkey", "ssh key", "signing key", "policy"), "4.9"),
        (("search", "code navigation", "language stat", "license detection", "copy detection", "blame"), "4.4"),
        (("webhook", "rest api", "graphql", "openapi", "sdk"), "4.8"),
        (("deployment", "environment", "bisect", "revert pipeline", "post-merge", "pre-deploy", "check run", "status check", "workflow", "runner", "artifact", "cache (actions"), "4.14"),
        (("issue",), "4.15"), (("project",), "4.15"),
        (("release", "wiki", "gist", "discussion", "notification", "profile", "star", "sponsor", "insight", "topic", "template repo", "archive (read-only)", "transfer"), "4.16"),
        (("render", "web ui", "web file editor", "mobile", "desktop"), "4.24"),
    ]
    for keys, a in rules:
        if any(k in t for k in keys):
            return a
    return default

GH_SECTION = {"0": "4.8", "1": "4.16", "2": "4.13", "3": "4.14", "4": "4.15", "5": "4.15", "6": "4.16",
    "7": "4.14", "8": "4.17", "9": "4.17", "10": "4.19", "11": "4.19", "12": "4.18", "13": "4.16",
    "14": "4.4", "15": "4.16", "16": "4.16", "17": "4.16", "18": "4.20", "19": "4.9", "20": "4.20",
    "21": "4.20", "22": "4.7", "23": "4.21", "24": "4.12", "25": "4.20"}
GH_FIXED = {"2", "4", "5", "7", "8", "9", "12", "18", "21", "23", "24"}  # don't re-route by keyword
SC_SECTION = {"1": "4.1", "2": "4.3", "3": "4.1", "4": "4.2", "5": "4.4", "6": "4.5", "7": "4.6",
    "8": "4.9", "9": "4.22", "10": "4.22", "11": "4.22", "12": "4.25", "13": "4.12", "14": "4.13",
    "15": "4.13", "16": "4.14", "17": "4.22"}
SC_FIXED = {"1", "2", "3", "4", "6", "7", "12", "13", "15", "16"}

rows = []
def add(area, feature, source, status):
    feature = re.sub(r"\s+", " ", feature).strip().strip("|").strip()
    if not feature:
        return
    rows.append({"area": area, "feature": feature[:240], "source": source, "v02_status": status})

# GitHub audit: "## N. Title" then "- [STATUS] text"
sec = None
for line in (R/"audit-github.md").read_text().splitlines():
    m = re.match(r"## (\d+)\. ", line)
    if m: sec = m.group(1); continue
    m = re.match(r"- \[(OK|PARTIAL|MISSING|EXCLUDED)\] (.+)", line)
    if m and sec is not None:
        d = GH_SECTION[sec]
        a = d if sec in GH_FIXED else area_kw(m.group(2).split(":")[0], d)
        add(a, m.group(2).split(" — ")[0].split(": ")[0] if len(m.group(2)) > 160 else m.group(2), "github", m.group(1))

# Scion/post audit: "## N. Title" then table rows "| item | source says | ours | add |"
sec = None; part = None
for line in (R/"audit-scion-and-post.md").read_text().splitlines():
    m = re.match(r"## (\d+)\. ", line)
    if m: sec = m.group(1); part = "table"; continue
    if line.startswith("## Scion CLI commands"): sec = None; part = "cli"; continue
    if line.startswith("## Scion RPCs"): sec = None; part = "rpc"; continue
    if not line.startswith("| ") or set(line.replace("|", "").strip()) <= set("-: "):
        continue
    cells = [c.strip() for c in line.strip().strip("|").split("|")]
    if cells[0] in ("Item", "Command (schema file)", "Scion service"):
        continue
    if part == "table" and sec in SC_SECTION and len(cells) >= 3:
        src = "post" if cells[1].startswith("A") else "scion"
        st = re.match(r"(MISSING|PARTIAL|DEFERRED|OK)", cells[2])
        d = SC_SECTION[sec]
        a = d if sec in SC_FIXED else area_kw(cells[0], d)
        add(a, cells[0], src, st.group(1) if st else "PARTIAL")
    elif part == "cli":
        add("4.7", "CLI command: " + cells[0], "scion", "MISSING" if "MISSING" in line else ("PARTIAL" if "PARTIAL" in line else "OK"))
    elif part == "rpc" and len(cells) >= 3:
        add("4.8" if not cells[0].startswith("Git") else "4.11", f"RPC {cells[0]}.{cells[1]}", "scion",
            "MISSING" if "None" in cells[2] else ("PARTIAL" if "missing" in cells[2].lower() else "OK"))

# Capabilities named directly in the post (each must hold in the finished product).
POST = [
 ("4.1", "Changesets store only files changed vs first parent, content-addressed"),
 ("4.1", "Server performs history queries, merges, rebases and commits"),
 ("4.1", "No fully materialized copy of a repo anywhere; every component scales independently"),
 ("4.1", "Operations are incremental and lock-free; cost scales with change size"),
 ("4.4", "Derived data computed recursively from parents, eagerly or lazily, new indexes without client changes"),
 ("4.4", "Copy detection index"), ("4.4", "Path-based search index"),
 ("4.5", "Three cache tiers (node daemon, regional cache, servers) answering identical RPCs"),
 ("4.5", "Each cache tier above 99% hit rate"), ("4.5", "Daemon starts from periodically refreshed snapshots"),
 ("4.6", "Sparse checkouts"), ("4.6", "Virtualized checkouts materializing files on demand"),
 ("4.1", "Write workloads skip the checkout entirely via API"),
 ("4.13", "Review tool with semantic approval constraints (comment-only vs sensitive logic)"),
 ("4.13", "Merge queue adapted for agent scale with no global merge lock"),
 ("4.14", "Post-merge and pre-deploy automated testing replaces merge locks"),
 ("4.14", "Automated bisect and revert pipelines using cheap programmatic history"),
 ("4.9", "Repository and per-directory access control"),
 ("4.25", "Hermetic simulations of every non-trivial interaction"), ("4.25", "Fuzzing"),
 ("4.22", "Sophisticated rate limiting"),
 ("4.19", "Component watcher agents (superfans) with telemetry access"),
 ("4.12", "Ingestion pipeline from git preserving ordering invariants"),
 ("4.5", "CI checkouts in seconds, not minutes"),
 ("4.23", "One-file CreateChangeset p50 <= 10 ms, p99 <= 50 ms, independent of repo size"),
 ("4.23", "Uncontended land p50 <= 30 ms, p99 <= 150 ms"),
 ("4.23", "Hot ref: >= 100 lands/s with 1,000 concurrent landers, p99 time-to-land <= 10 s"),
 ("4.23", "Sparse checkout of 1,000 files <= 1 s warm, <= 3 s cold"),
 ("4.23", "Log, blame, diff p99 <= 200 ms"),
 ("4.23", "Event delivery p99 <= 1 s"),
 ("4.23", ">= 10,000 concurrent agent sessions per cell"),
 ("4.23", "Availability: reads 99.99%, writes 99.95% per cell per month"),
 ("4.11", "Native git commit format with change-id trailer and commit-msg hook"),
 ("4.11", "Git tree ordering and author-name sanitising; collision-detecting SHA-1; one object format per repo"),
 ("4.11", "Protocol v2 capabilities, minimum supported git version, generation-number negotiation"),
 ("4.11", "Partial clone filters blob:none, blob:limit, tree:<n>; object-info"),
 ("4.11", "Seed packs via packfile-uris and bundle-uri; delta hints and thin packs"),
 ("4.11", "receive-pack capabilities, atomic push, push ingest checks, ng/ERR error mapping"),
 ("4.11", "Virtual refs for changesets; replace refs for redacted commits"),
 ("4.11", "Git storage tables, gateway config and per-repo git enable switch"),
 ("4.11", "Credential helper, HTTP Basic with token, setup and preflight commands"),
 ("4.11", "Git over SSH"),
 ("4.11", "Gateway metrics"),
 ("4.20", "Cloud: multi-tenant signup and onboarding"),
 ("4.20", "Cloud: region selection and data residency"),
 ("4.20", "Cloud: usage metering and billing collection"),
 ("4.22", "Cloud: managed upgrades rolled out cell by cell"),
 ("4.14", "Hosted runner and workspace fleet, same software available to self-hosters"),
 ("4.25", "Every suite runs against both a self-hosted server and a cloud staging cell"),
 ("4.14", "Workflow corpus (top 500 starred Actions repos + kubernetes) matches GitHub outcomes 100%"),
 ("4.12", "kubernetes/kubernetes migrates with a clean fidelity report, faster than 76 h"),
 ("4.5", "Signed CDN URLs for immutable object reads, issued after authorization, never for redacted objects"),
 ("4.3", "Hot and cold storage tiers with transparent reads and unchanged ids"),
 ("4.22", "Read-your-writes per session via min_version on replica reads"),
 ("4.1", "Required land coalescer: one fenced owner per protected ref, queued lands committed as one CAS"),
 ("4.23", "Production gate: metadata benchmark repeated on three zones with synchronous replication before the cloud serves customers"),
]
for a, f in POST:
    add(a, f, "post", "see audit")

out = root/"docs/coverage/features.csv"
counters = {}
with out.open("w", newline="") as fh:
    w = csv.DictWriter(fh, fieldnames=["id", "area", "feature", "source", "v02_status", "verification", "scenarios"])
    w.writeheader()
    for r in rows:
        key = r["area"].replace(".", "")
        counters[key] = counters.get(key, 0) + 1
        r["id"] = f"F{key}-{counters[key]:03d}"
        r["verification"] = VERIF.get(r["area"], "agent-scenario")
        r["scenarios"] = " ".join(SCEN.get(r["area"], []))
        w.writerow(r)
print(f"{len(rows)} rows -> {out.relative_to(root)}")
