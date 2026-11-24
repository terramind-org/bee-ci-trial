#!/usr/bin/env python3
"""Concurrent land load against a Beehive endpoint, and the checks that every land stuck.

Used by scripts/check-scaleout.sh against the load balancer of deploy/compose.scale.yml, and
usable against any Beehive deployment (docs/scaling.md). Standard library only; it speaks the
Connect HTTP/JSON protocol (SPEC 12.1).

  land-load.py run    --endpoint URL --repo ORG/NAME --clients 32 --lands 320 --out run.json
                      [--hook 0.5='docker kill …'] [--hook 0.8='docker start …']
  land-load.py prepare --endpoint URL --repo ORG/NAME --count N --prefix DIR
  land-load.py verify --endpoint URL --repo ORG/NAME --results run.json \
                      --replica http://127.0.0.1:27421 [--replica …] --out verify.json

`run`: every client lands its own files on `main`, one changeset per file, each created on the
tip `main` had when the run started, so every land is rebased by the server (SPEC 7.4). Per
land: upload the blob, create the changeset, land it. A request that fails with a connection
error, a 502/503/504 from a proxy, or a retryable Connect code (unavailable,
resource_exhausted, aborted, deadline_exceeded) is retried with the same ids, so a retry of a
land that already happened is answered `skipped` by the server's landed index. A land counts as
acknowledged when Land answers 200. Hooks run once, in the background, when the given fraction
of lands has been acknowledged.

`verify`: reads main through the endpoint and from each replica directly (ResolveRef with
`min_version` = the final version), and checks: every replica reports the same tip; history is
linear; every acknowledged land's change is in history exactly once, at the changeset Land
answered; every file is in the tip's tree with its blob.

The token comes from the BEE_TOKEN environment variable, never from an argument.
"""

import argparse
import base64
import http.client
import json
import os
import random
import socket
import subprocess
import sys
import threading
import time
import urllib.parse

# ----- BLAKE3 of short inputs ------------------------------------------------------------------
# Blob ids are BLAKE3 over the bytes (SPEC 4.1). Python's hashlib has no BLAKE3, so this is the
# single-block case of the BLAKE3 specification: inputs of at most 64 bytes, which is all the
# load files need. Checked against the published test vectors at start-up.

_IV = (0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19)
_PERM = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
_M32 = 0xFFFFFFFF


def _rotr(x, n):
    return ((x >> n) | (x << (32 - n))) & _M32


def _g(s, a, b, c, d, x, y):
    s[a] = (s[a] + s[b] + x) & _M32
    s[d] = _rotr(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & _M32
    s[b] = _rotr(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b] + y) & _M32
    s[d] = _rotr(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & _M32
    s[b] = _rotr(s[b] ^ s[c], 7)


def blake3_short(data: bytes) -> bytes:
    if len(data) > 64:
        raise ValueError("blake3_short hashes at most 64 bytes")
    block = data + b"\0" * (64 - len(data))
    m = [int.from_bytes(block[i : i + 4], "little") for i in range(0, 64, 4)]
    # chaining value IV, counter 0, flags CHUNK_START | CHUNK_END | ROOT
    s = list(_IV) + list(_IV[:4]) + [0, 0, len(data), 1 | 2 | 8]
    for r in range(7):
        _g(s, 0, 4, 8, 12, m[0], m[1])
        _g(s, 1, 5, 9, 13, m[2], m[3])
        _g(s, 2, 6, 10, 14, m[4], m[5])
        _g(s, 3, 7, 11, 15, m[6], m[7])
        _g(s, 0, 5, 10, 15, m[8], m[9])
        _g(s, 1, 6, 11, 12, m[10], m[11])
        _g(s, 2, 7, 8, 13, m[12], m[13])
        _g(s, 3, 4, 9, 14, m[14], m[15])
        if r < 6:
            m = [m[i] for i in _PERM]
    return b"".join(((s[i] ^ s[i + 8]) & _M32).to_bytes(4, "little") for i in range(8))


assert blake3_short(b"").hex() == "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
assert blake3_short(b"abc").hex() == "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"


def blob_id_raw(data: bytes) -> bytes:
    # algorithm byte 0x01 (BLAKE3), kind byte 0x01 (blob), then the digest (SPEC 4.2)
    return b"\x01\x01" + blake3_short(data)


def b64(b: bytes) -> str:
    return base64.b64encode(b).decode()


# ----- Connect over HTTP/1.1 -------------------------------------------------------------------

RETRY_CODES = {"unavailable", "resource_exhausted", "aborted", "deadline_exceeded"}
PROXY_STATUSES = {502, 503, 504}


class CallError(Exception):
    def __init__(self, what, retryable, code="", status=0):
        super().__init__(what)
        self.retryable = retryable
        self.code = code
        self.status = status


class Client:
    """One keep-alive connection to an endpoint; reconnects after any transport error."""

    def __init__(self, endpoint, token, timeout=60.0):
        u = urllib.parse.urlsplit(endpoint)
        if u.scheme != "http":
            raise SystemExit(f"land-load: only http:// endpoints are supported, got {endpoint}")
        self.host, self.port = u.hostname, u.port or 80
        self.token = token
        self.timeout = timeout
        self.conn = None

    def _post(self, path, body, ctype):
        if self.conn is None:
            self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        headers = {"Content-Type": ctype, "Authorization": f"Bearer {self.token}"}
        try:
            self.conn.request("POST", path, body=body, headers=headers)
            resp = self.conn.getresponse()
            data = resp.read()
            upstream = resp.getheader("X-Bee-Upstream", "")
            if resp.getheader("Connection", "").lower() == "close":
                self.conn.close()
                self.conn = None
            return resp.status, data, upstream
        except (OSError, http.client.HTTPException, socket.timeout) as e:
            if self.conn is not None:
                self.conn.close()
            self.conn = None
            raise CallError(f"transport: {type(e).__name__}: {e}", True) from e

    @staticmethod
    def _error(status, data):
        try:
            err = json.loads(data)
        except ValueError:
            err = None
        if isinstance(err, dict) and "code" in err:
            code = err.get("code", "")
            reason = ""
            for d in err.get("details") or []:
                v = d.get("debug") if isinstance(d, dict) else None
                if isinstance(v, dict) and v.get("reason"):
                    reason = v["reason"]
            msg = err.get("message", "")
            return CallError(f"HTTP {status} {code} {reason} {msg}".strip(), code in RETRY_CODES, code, status)
        text = data[:200].decode("utf-8", "replace")
        return CallError(f"HTTP {status}: {text}", status in PROXY_STATUSES, "", status)

    def unary(self, method, msg):
        status, data, upstream = self._post(f"/bee.v1.{method}", json.dumps(msg).encode(), "application/json")
        if status != 200:
            raise self._error(status, data)
        return json.loads(data or b"{}"), upstream

    def upload(self, items):
        """Half-duplex client stream (SPEC 12.1): one envelope per item, then the answer."""
        body = b"".join(
            b"\x00" + len(p).to_bytes(4, "big") + p for p in (json.dumps(i).encode() for i in items)
        )
        status, data, upstream = self._post("/bee.v1.WriteService/UploadObjects", body, "application/connect+json")
        if status != 200:
            raise self._error(status, data)
        out, pos = None, 0
        while pos + 5 <= len(data):
            flags, n = data[pos], int.from_bytes(data[pos + 1 : pos + 5], "big")
            payload = data[pos + 5 : pos + 5 + n]
            pos += 5 + n
            doc = json.loads(payload or b"{}")
            if flags & 0x02:
                if doc.get("error"):
                    e = doc["error"]
                    code = e.get("code", "")
                    raise CallError(f"upload: {code} {e.get('message', '')}", code in RETRY_CODES, code, status)
            else:
                out = doc
        if out is None:
            raise CallError("upload: no response message", True)
        return out, upstream


def with_retries(stats, what, fn, budget_s=180.0):
    """Runs fn until it succeeds, retrying retryable failures for up to budget_s seconds."""
    t0 = time.monotonic()
    attempt = 0
    while True:
        attempt += 1
        try:
            return fn(), attempt
        except CallError as e:
            if not e.retryable or time.monotonic() - t0 > budget_s:
                raise CallError(f"{what}: {e} (after {attempt} attempts)", False, e.code, e.status) from e
            stats.retry(what, e)
            time.sleep(min(2.0, 0.05 * (2 ** min(attempt, 6))) * (0.5 + random.random()))


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.retries = {}
        self.retry_samples = {}

    def retry(self, what, e):
        key = f"{what}: {e.code or (f'HTTP {e.status}' if e.status else 'transport')}"
        with self.lock:
            self.retries[key] = self.retries.get(key, 0) + 1
            self.retry_samples.setdefault(key, str(e)[:300])


def pct(sorted_vals, p):
    if not sorted_vals:
        return 0.0
    i = max(1, min(len(sorted_vals), -(-len(sorted_vals) * p // 1))) - 1
    return sorted_vals[int(i)]


def repo_ref(full):
    return {"fullName": full}


# ----- run -------------------------------------------------------------------------------------


def cmd_run(a):
    token = os.environ.get("BEE_TOKEN", "")
    if not token:
        raise SystemExit("land-load: set BEE_TOKEN")
    run = a.run_id or os.urandom(4).hex()
    if not (len(run) == 8 and all(c in "0123456789abcdef" for c in run)):
        raise SystemExit("land-load: --run-id must be 8 lowercase hex digits")
    hooks = []
    for h in a.hook:
        frac, _, cmd = h.partition("=")
        hooks.append({"at": float(frac), "cmd": cmd, "fired": False})
    hooks.sort(key=lambda h: h["at"])

    head = Client(a.endpoint, token)
    tip, _ = head.unary("ObjectService/ResolveRef", {"repo": repo_ref(a.repo), "name": "main"})
    base = tip["ref"]["target"]["raw"]
    base_version = int(tip["ref"].get("version", 0))

    stats = Stats()
    lock = threading.Lock()
    acks, failures, hook_log = [], [], []
    acked = [0]
    t_start = time.monotonic()

    def fire_hooks():
        frac = acked[0] / a.lands
        for h in hooks:
            if not h["fired"] and frac >= h["at"]:
                h["fired"] = True
                threading.Thread(target=run_hook, args=(h, acked[0]), daemon=True).start()

    def run_hook(h, at_acked):
        t0 = time.monotonic()
        p = subprocess.run(h["cmd"], shell=True, capture_output=True, text=True)
        rec = {
            "cmd": h["cmd"],
            "at_fraction": h["at"],
            "at_acked": at_acked,
            "started_s": round(t0 - t_start, 3),
            "took_s": round(time.monotonic() - t0, 3),
            "exit": p.returncode,
            "output": (p.stdout + p.stderr).strip()[-500:],
        }
        with lock:
            hook_log.append(rec)
        print(f"land-load: hook at {at_acked} acked ({h['at']:.0%}): {h['cmd']} -> exit {p.returncode}", flush=True)

    def worker(c, indices):
        cl = Client(a.endpoint, token)
        for i in indices:
            path = f"scale/{run}/c{c:02d}/l{i:04d}.txt"
            content = f"land {run} c{c:02d} l{i:04d}\n".encode()
            blob = blob_id_raw(content)
            change = bytes.fromhex(run) + c.to_bytes(2, "big") + i.to_bytes(4, "big") + b"\x5a\x5a\x5a\x5a\x5a\x5a"
            t0 = time.monotonic()
            try:
                item = {"repo": repo_ref(a.repo), "id": {"raw": b64(blob)}, "offset": "0", "data": b64(content), "last": True}
                _, up_attempts = with_retries(stats, "UploadObjects", lambda: cl.upload([item]))
                req = {
                    "repo": repo_ref(a.repo),
                    "parents": [{"raw": base}],
                    "delta": [{"path": b64(path.encode()), "put": {"mode": "MODE_FILE", "blob": {"raw": b64(blob)}, "size": str(len(content))}}],
                    "changeId": {"raw": b64(change)},
                    "message": f"Land {path}",
                    "authorTime": {"name": "land-load", "time": "2026-11-15T00:00:00Z", "tzOffsetMinutes": 0},
                }
                (cs, _), cs_attempts = with_retries(stats, "CreateChangeset", lambda: cl.unary("WriteService/CreateChangeset", req))
                source = cs["changeset"]["id"]["raw"]
                land_req = {"repo": repo_ref(a.repo), "ref": "main", "head": {"raw": source}}
                timing = {}

                def land():
                    t = time.monotonic()
                    out = cl.unary("WriteService/Land", land_req)
                    timing["land_s"] = time.monotonic() - t
                    return out

                (resp, upstream), land_attempts = with_retries(stats, "Land", land)
            except CallError as e:
                with lock:
                    failures.append({"client": c, "i": i, "path": path, "error": str(e)})
                continue
            landed = (resp.get("landed") or [{}])[0]
            rec = {
                "client": c,
                "i": i,
                "path": path,
                "blob": b64(blob),
                "change_id": b64(change),
                "source": source,
                "result": landed.get("result", {}).get("raw", ""),
                "skipped": bool(landed.get("skipped", False)),
                "version": int(resp.get("ref", {}).get("version", 0)),
                "upstream": upstream,
                "land_ms": round(timing["land_s"] * 1000, 2),
                "e2e_ms": round((time.monotonic() - t0) * 1000, 2),
                "attempts": {"upload": up_attempts, "create": cs_attempts, "land": land_attempts},
            }
            with lock:
                acks.append(rec)
                acked[0] += 1
                fire_hooks()

    per = [[] for _ in range(a.clients)]
    for i in range(a.lands):
        per[i % a.clients].append(i)
    threads = [threading.Thread(target=worker, args=(c, per[c])) for c in range(a.clients)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.monotonic() - t_start
    # A hook still running (a slow `docker start`) is waited for, so its record is complete.
    deadline = time.monotonic() + 120
    while len(hook_log) < sum(1 for h in hooks if h["fired"]) and time.monotonic() < deadline:
        time.sleep(0.1)

    land_ms = sorted(r["land_ms"] for r in acks)
    e2e_ms = sorted(r["e2e_ms"] for r in acks)
    upstreams = {}
    for r in acks:
        upstreams[r["upstream"] or "?"] = upstreams.get(r["upstream"] or "?", 0) + 1
    summary = {
        "run": run,
        "endpoint": a.endpoint,
        "repo": a.repo,
        "clients": a.clients,
        "lands": a.lands,
        "base_version": base_version,
        "acked": len(acks),
        "failed": len(failures),
        "skipped_on_retry": sum(1 for r in acks if r["skipped"]),
        "wall_s": round(wall, 3),
        "lands_per_s": round(len(acks) / wall, 2) if wall > 0 else 0,
        "land_ms": {"p50": pct(land_ms, 0.50), "p99": pct(land_ms, 0.99), "max": land_ms[-1] if land_ms else 0},
        "e2e_ms": {"p50": pct(e2e_ms, 0.50), "p99": pct(e2e_ms, 0.99), "max": e2e_ms[-1] if e2e_ms else 0},
        "retries": stats.retries,
        "retry_samples": stats.retry_samples,
        "land_upstreams": upstreams,
        "hooks": sorted(hook_log, key=lambda h: h["started_s"]),
        "failures": failures,
        "acks": sorted(acks, key=lambda r: (r["client"], r["i"])),
    }
    with open(a.out, "w") as f:
        json.dump(summary, f, indent=1)
    s = summary
    print(
        f"land-load: {s['acked']}/{s['lands']} lands acknowledged by {s['clients']} clients in {s['wall_s']}s "
        f"({s['lands_per_s']} lands/s); Land p50 {s['land_ms']['p50']} ms, p99 {s['land_ms']['p99']} ms; "
        f"upload+create+land p50 {s['e2e_ms']['p50']} ms, p99 {s['e2e_ms']['p99']} ms",
        flush=True,
    )
    print(f"land-load: Land answered by {json.dumps(upstreams, sort_keys=True)}; retries {json.dumps(stats.retries, sort_keys=True)}", flush=True)
    for f_ in failures[:10]:
        print(f"land-load: FAILED {f_['path']}: {f_['error']}", file=sys.stderr)
    for h in hooks:
        if not h["fired"]:
            print(f"land-load: hook at {h['at']:.0%} never fired", file=sys.stderr)
    ok = not failures and all(h["fired"] for h in hooks) and all(h["exit"] == 0 for h in hook_log)
    return 0 if ok else 1


# ----- prepare ---------------------------------------------------------------------------------


def cmd_prepare(a):
    """Creates one changeset per file on the tip of main without landing it, and prints one JSON
    line per changeset ({path, source}), for landing it with another client (grpcurl)."""
    token = os.environ.get("BEE_TOKEN", "")
    if not token:
        raise SystemExit("land-load: set BEE_TOKEN")
    cl = Client(a.endpoint, token)
    repo = repo_ref(a.repo)
    stats = Stats()
    (tip, _), _ = with_retries(stats, "ResolveRef", lambda: cl.unary("ObjectService/ResolveRef", {"repo": repo, "name": "main"}))
    for i in range(a.count):
        path = f"{a.prefix}/{i:04d}.txt"
        content = f"{a.prefix} {i}\n".encode()
        if len(content) > 64:
            raise SystemExit("land-load: --prefix is too long")
        blob = blob_id_raw(content)
        item = {"repo": repo, "id": {"raw": b64(blob)}, "offset": "0", "data": b64(content), "last": True}
        with_retries(stats, "UploadObjects", lambda: cl.upload([item]))
        req = {
            "repo": repo,
            "parents": [{"raw": tip["ref"]["target"]["raw"]}],
            "delta": [{"path": b64(path.encode()), "put": {"mode": "MODE_FILE", "blob": {"raw": b64(blob)}, "size": str(len(content))}}],
            "changeId": {"raw": b64(os.urandom(16))},
            "message": f"Land {path}",
            "authorTime": {"name": "land-load", "time": "2026-11-15T00:00:00Z", "tzOffsetMinutes": 0},
        }
        (cs, _), _ = with_retries(stats, "CreateChangeset", lambda: cl.unary("WriteService/CreateChangeset", req))
        print(json.dumps({"path": path, "source": cs["changeset"]["id"]["raw"]}), flush=True)
    return 0


# ----- verify ----------------------------------------------------------------------------------


def cmd_verify(a):
    token = os.environ.get("BEE_TOKEN", "")
    if not token:
        raise SystemExit("land-load: set BEE_TOKEN")
    with open(a.results) as f:
        res = json.load(f)
    acks = res["acks"]
    problems = []
    lb = Client(a.endpoint, token)
    repo = repo_ref(a.repo)

    tip, _ = lb.unary("ObjectService/ResolveRef", {"repo": repo, "name": "main"})
    tip_ref = tip["ref"]
    tip_version = int(tip_ref["version"])
    max_acked = max((r["version"] for r in acks), default=0)
    if tip_version < max_acked:
        problems.append(f"main is at version {tip_version}, below the highest acknowledged version {max_acked}")

    replicas = {}
    for url in a.replica:
        t0 = time.monotonic()
        r, _ = Client(url, token).unary(
            "ObjectService/ResolveRef", {"repo": repo, "name": "main", "minVersion": str(tip_version)}
        )
        ref = r["ref"]
        replicas[url] = {
            "version": int(ref["version"]),
            "target": ref["target"]["raw"],
            "ms": round((time.monotonic() - t0) * 1000, 2),
        }
        if int(ref["version"]) != tip_version or ref["target"]["raw"] != tip_ref["target"]["raw"]:
            problems.append(f"replica {url} reports main at version {ref['version']}, the balancer at {tip_version}")

    # History of main, newest first.
    log, token_ = [], ""
    while True:
        req = {"repo": repo, "from": {"expr": "main"}, "page": {"pageSize": 500, "pageToken": token_}, "minVersion": str(tip_version)}
        page, _ = lb.unary("QueryService/Log", req)
        log.extend(page.get("changesets") or [])
        token_ = (page.get("page") or {}).get("nextPageToken", "")
        if not token_:
            break
    ids = [c["id"]["raw"] for c in log]
    for k, c in enumerate(log):
        parents = [p["raw"] for p in c.get("parents") or []]
        want = [ids[k + 1]] if k + 1 < len(log) else []
        if parents != want:
            problems.append(f"history is not linear at changeset {k} from the tip: parents {parents}, expected {want}")
            break
    if log and log[0]["id"]["raw"] != tip_ref["target"]["raw"]:
        problems.append("the newest log entry is not the tip of main")
    by_change = {}
    for c in log:
        cid = (c.get("changeId") or {}).get("raw", "")
        by_change.setdefault(cid, []).append(c["id"]["raw"])
    seen_change = set()
    for r in acks:
        if r["change_id"] in seen_change:
            problems.append(f"{r['path']} was acknowledged twice by the load client")
        seen_change.add(r["change_id"])
        hits = by_change.get(r["change_id"], [])
        if len(hits) != 1:
            problems.append(f"{r['path']}: its change is in history {len(hits)} times, expected exactly once")
        elif hits[0] != r["result"]:
            problems.append(f"{r['path']}: history has its change at another changeset than Land answered")

    # Every file at the tip, with its blob.
    tree, token_ = {}, ""
    prefix = f"scale/{res['run']}"
    while True:
        req = {"repo": repo, "at": {"expr": "main"}, "path": b64(prefix.encode()), "recursive": True, "page": {"pageSize": 1000, "pageToken": token_}}
        page, _ = lb.unary("QueryService/ListTree", req)
        for e in page.get("entries") or []:
            p = base64.b64decode(e["path"]).decode()
            if not p.startswith(prefix + "/"):
                p = f"{prefix}/{p}"
            if e.get("mode", "") in ("MODE_FILE", 1):
                tree[p] = (e.get("id") or {}).get("raw", "")
        token_ = (page.get("page") or {}).get("nextPageToken", "")
        if not token_:
            break
    missing = [r["path"] for r in acks if tree.get(r["path"]) != r["blob"]]
    if missing:
        problems.append(f"{len(missing)} acknowledged files are missing at the tip or have another blob, e.g. {missing[:3]}")
    extra = set(tree) - {r["path"] for r in acks}
    if extra:
        problems.append(f"{len(extra)} files under {prefix} at the tip were never acknowledged, e.g. {sorted(extra)[:3]}")

    out = {
        "tip_version": tip_version,
        "tip": tip_ref["target"]["raw"],
        "log_count": len(log),
        "acked": len(acks),
        "files_at_tip": len(tree),
        "replicas": replicas,
        "problems": problems,
    }
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1)
    print(
        f"land-load: verify: main at version {tip_version}, {len(log)} changesets in linear history, "
        f"{len(acks)} acknowledged lands each in history once, {len(tree)} files at the tip; "
        + ", ".join(f"{u} at v{r['version']} ({r['ms']} ms)" for u, r in replicas.items()),
        flush=True,
    )
    for p in problems:
        print(f"land-load: verify: PROBLEM: {p}", file=sys.stderr)
    return 1 if problems else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--endpoint", required=True)
    r.add_argument("--repo", required=True)
    r.add_argument("--clients", type=int, default=32)
    r.add_argument("--lands", type=int, default=320)
    r.add_argument("--run-id", default="")
    r.add_argument("--hook", action="append", default=[], help="FRACTION=COMMAND, run once at that fraction")
    r.add_argument("--out", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("--endpoint", required=True)
    p.add_argument("--repo", required=True)
    p.add_argument("--count", type=int, default=1)
    p.add_argument("--prefix", required=True)
    v = sub.add_parser("verify")
    v.add_argument("--endpoint", required=True)
    v.add_argument("--repo", required=True)
    v.add_argument("--results", required=True)
    v.add_argument("--replica", action="append", default=[])
    v.add_argument("--out", required=True)
    a = ap.parse_args()
    if a.cmd == "run" and (a.clients < 1 or a.lands < a.clients):
        raise SystemExit("land-load: need --clients >= 1 and --lands >= --clients")
    sys.exit({"run": cmd_run, "prepare": cmd_prepare, "verify": cmd_verify}[a.cmd](a))


if __name__ == "__main__":
    main()
