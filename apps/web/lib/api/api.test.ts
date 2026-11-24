import { create, toBinary } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { ErrorDetailSchema } from "../gen/bee/v1/common_pb";
import { createClient } from "./client";
import { FixtureStore, fixtureRpc } from "./fixture";
import { httpRpc } from "./http";
import { base64ToText, bytesToBase64, changeIdText, objectIdText, parseObjectId, pathBytes, pathText, textToBase64 } from "./ids";
import { BeeError } from "./transport";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);
const repo = { fullName: "acme/api" };

const TOKEN = "mara-token-0001";

function fixture(clock = () => 0) {
  const store = new FixtureStore({ anchor: ANCHOR, clock, live: false, logMs: 100, importMs: 1000 });
  return { store, api: createClient(fixtureRpc(store, TOKEN)) };
}

/** A Connect streaming envelope: flags, 4-byte big-endian length, JSON payload. */
function envelope(payload: unknown, flags = 0): Uint8Array<ArrayBuffer> {
  const body = new TextEncoder().encode(JSON.stringify(payload));
  const out = new Uint8Array(5 + body.length);
  out[0] = flags;
  new DataView(out.buffer).setUint32(1, body.length, false);
  out.set(body, 5);
  return out;
}

function detail(reason: string, message: string, extra: Partial<{ policyDenial: { rule: string; missing: string[] } }> = {}) {
  const value = bytesToBase64(toBinary(ErrorDetailSchema, create(ErrorDetailSchema, { reason, message, payload: extra.policyDenial ? { case: "policyDenial", value: extra.policyDenial } : { case: undefined } }))).replace(/=+$/, "");
  return { type: "bee.v1.ErrorDetail", value, debug: { reason, message } };
}

describe("ids", () => {
  it("round-trips base64 including padding and UTF-8", () => {
    for (const s of ["", "a", "ab", "abc", "src/auth/session.ts", "naïve/日本.md"]) expect(base64ToText(textToBase64(s))).toBe(s);
    expect(bytesToBase64(new Uint8Array([255, 0, 1]))).toBe("/wAB");
  });

  it("formats and parses object ids in SPEC §3.1 text form", () => {
    const text = "c1_" + "ab".repeat(32);
    const id = parseObjectId(text);
    expect(id).not.toBeNull();
    expect(objectIdText(id ?? undefined)).toBe(text);
    expect(objectIdText(id ?? undefined, true)).toBe("c1_ababababab");
    expect(parseObjectId("c1_xyz")).toBeNull();
  });

  it("formats change ids as k_<32 hex>", () => {
    expect(changeIdText({ raw: bytesToBase64(new Uint8Array(16).fill(15)) })).toBe("k_" + "0f".repeat(16));
  });
});

describe("http adapter (connect-web over the generated descriptors)", () => {
  it("posts proto3 JSON to the Connect route with the bearer token", async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    const rpc = httpRpc({
      baseUrl: "https://bee.test/",
      token: "tok",
      fetch: (async (url: string, init: RequestInit) => {
        calls.push({ url: String(url), init });
        return new Response(JSON.stringify({ id: "x", number: "7" }), { status: 200, headers: { "Content-Type": "application/json" } });
      }) as unknown as typeof fetch,
    });
    const cr = await createClient(rpc).change.getChange({ repo, id: "x" });
    expect(cr).toMatchObject({ id: "x", number: "7", title: "", revisions: [], state: "CHANGE_STATE_UNSPECIFIED" });
    expect(calls[0]?.url).toBe("https://bee.test/bee.v1.ChangeService/GetChange");
    expect(new Headers(calls[0]?.init.headers).get("Authorization")).toBe("Bearer tok");
    expect(JSON.parse(new TextDecoder().decode(calls[0]?.init.body as Uint8Array))).toEqual({ repo: { fullName: "acme/api" }, id: "x" });
  });

  it("turns Connect errors into BeeError with the SPEC reason from bee.v1.ErrorDetail", async () => {
    const rpc = httpRpc({
      baseUrl: "https://bee.test",
      fetch: (async () =>
        new Response(JSON.stringify({ code: "failed_precondition", message: "denied", details: [detail("POLICY_DENIED", "denied", { policyDenial: { rule: "land.main", missing: ["check:test"] } })] }), {
          status: 400,
          headers: { "Content-Type": "application/json" },
        })) as unknown as typeof fetch,
    });
    const err = await createClient(rpc).write.land({ repo, ref: "main" }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(BeeError);
    expect((err as BeeError).reason).toBe("POLICY_DENIED");
    expect((err as BeeError).detail.policyDenial?.missing).toEqual(["check:test"]);
  });

  it("reads a server stream and stops at the end envelope", async () => {
    const body = new Uint8Array([...envelope({ seq: "1", kind: "a" }), ...envelope({ seq: "2", kind: "b" }), ...envelope({}, 2)]);
    const rpc = httpRpc({ baseUrl: "https://bee.test", fetch: (async () => new Response(body, { status: 200, headers: { "Content-Type": "application/connect+json" } })) as unknown as typeof fetch });
    const seen: string[] = [];
    for await (const ev of createClient(rpc).event.subscribe({ repo })) seen.push(ev.seq);
    expect(seen).toEqual(["1", "2"]);
  });

  it("surfaces an error carried in the end envelope", async () => {
    const body = envelope({ error: { code: "not_found", message: "gone", details: [detail("RUN_NOT_FOUND", "gone")] } }, 2);
    const rpc = httpRpc({ baseUrl: "https://bee.test", fetch: (async () => new Response(body, { status: 200, headers: { "Content-Type": "application/connect+json" } })) as unknown as typeof fetch });
    const it = createClient(rpc).workflow.streamLogs({ repo, runId: "r", jobId: "j" });
    await expect((async () => { for await (const _ of it) void _; })()).rejects.toMatchObject({ reason: "RUN_NOT_FOUND" });
  });

  it("reports an unreachable server as UNAVAILABLE", async () => {
    const rpc = httpRpc({ baseUrl: "https://bee.test", fetch: (async () => { throw new TypeError("fetch failed"); }) as unknown as typeof fetch });
    await expect(createClient(rpc).org.getOrg({ nameOrId: "acme" })).rejects.toMatchObject({ code: "unavailable", retryable: true });
  });
});

describe("fixture: determinism", () => {
  it("builds identical worlds from identical options", async () => {
    const a = fixture();
    const b = fixture();
    const la = await a.api.change.listChanges({ repo });
    const lb = await b.api.change.listChanges({ repo });
    expect(la).toEqual(lb);
    expect(a.store.events.map((e) => e.kind + e.seq)).toEqual(b.store.events.map((e) => e.kind + e.seq));
  });

  it("has a rich enough world for every screen", async () => {
    const { api } = fixture();
    expect((await api.repo.listRepos({ org: "acme" })).repos.map((r) => r.fullName)).toEqual(["acme/api", "acme/web", "acme/infra", "acme/sdk"]);
    expect((await api.change.listChanges({ repo })).changes.length).toBeGreaterThanOrEqual(8);
    expect((await api.agent.listAgents({ org: "acme" })).agents.map((a) => a.principal.runtime)).toEqual(expect.arrayContaining(["claude-code", "codex"]));
    expect((await api.lease.listLeases({ repo })).leases.length).toBeGreaterThan(2);
    expect((await api.workflow.listRuns({ repo })).runs.some((r) => r.status === "running")).toBe(true);
    expect((await api.issue.listIssues({ repo })).issues.some((i) => i.subIssuesTotal > 0)).toBe(true);
    expect((await api.release.listReleases({ repo })).releases.length).toBe(3);
    expect((await api.security.listAlerts({ repo })).alerts.length).toBe(6);
    expect((await api.audit.listAudit({ org: "acme" })).chainVerified).toBe(true);
  });
});

describe("fixture: change requests and landing", () => {
  it("classifies revisions with the highest-risk diff class", async () => {
    const { api } = fixture();
    const { changes } = await api.change.listChanges({ repo });
    const byTitle = (t: string) => changes.find((c) => c.title.startsWith(t));
    expect(byTitle("Rotate session")?.revisions.at(-1)?.diffClass).toBe("sensitive");
    expect(byTitle("Document the webhook")?.revisions.at(-1)?.diffClass).toBe("docs");
    expect(byTitle("Bump zod")?.revisions.at(-1)?.diffClass).toBe("dependency");
    expect(byTitle("Fix rounding")?.revisions.at(-1)?.diffClass).toBe("logic");
  });

  it("refuses to land a sensitive change with one human approval, then allows it with two", async () => {
    const { api } = fixture();
    const cr = (await api.change.listChanges({ repo })).changes.find((c) => c.title.startsWith("Rotate session"));
    if (!cr) throw new Error("missing CR");
    const before = await api.change.evaluateLanding({ repo, changeRequestId: cr.id });
    expect(before.allowed).toBe(false);
    expect(before.missing.join()).toMatch(/approval:human:sensitive \(1 of 2\)/);
    await expect(api.write.land({ repo, ref: "main", changeRequestId: cr.id })).rejects.toMatchObject({ reason: "POLICY_DENIED" });
    // Mara (the default caller) approves the current revision.
    await api.change.submitReview({ repo, changeRequestId: cr.id, revision: 3, verdict: "VERDICT_APPROVE", body: "ok", comments: [] });
    const after = await api.change.evaluateLanding({ repo, changeRequestId: cr.id });
    expect(after.allowed).toBe(true);
    const landed = await api.write.land({ repo, ref: "main", changeRequestId: cr.id });
    expect(landed.ref?.version).toBe("11");
    expect((await api.change.getChange({ repo, id: cr.id })).state).toBe("CHANGE_STATE_LANDED");
  });

  it("applying a suggestion makes a new revision and makes old approvals stale", async () => {
    const { api, store } = fixture();
    const cr = (await api.change.listChanges({ repo })).changes.find((c) => c.title.startsWith("Rotate session"));
    if (!cr) throw new Error("missing CR");
    const { threads } = await api.change.listThreads({ repo, changeRequestId: cr.id });
    const withSuggestion = threads.filter((t) => t.comments.some((c) => c.suggestion) && !t.resolved);
    expect(withSuggestion).toHaveLength(1);
    const commentIds = withSuggestion.flatMap((t) => t.comments.filter((c) => c.suggestion).map((c) => c.id));
    const next = await api.change.applySuggestions({ repo, changeRequestId: cr.id, commentIds });
    expect(next.revisions).toHaveLength(4);
    const text = await api.readText({ repo, at: { expr: "ws/triage/rotate-sessions" }, path: pathBytes("src/auth/session.ts") });
    expect(text.text).toContain("const next: Session = {");
    store.step(); // checks finish
    const ev = await api.change.evaluateLanding({ repo, changeRequestId: cr.id });
    expect(ev.missing.join()).toMatch(/0 of 2/);
  });

  it("refuses review on a stale revision with a retryable reason", async () => {
    const { api } = fixture();
    const cr = (await api.change.listChanges({ repo })).changes.find((c) => c.title.startsWith("Rotate session"));
    const err = await api.change.submitReview({ repo, changeRequestId: cr?.id ?? "", revision: 1, verdict: "VERDICT_APPROVE", body: "", comments: [] }).catch((e: unknown) => e);
    expect(err).toMatchObject({ reason: "STALE_REVISION", retryable: true });
  });

  it("diffs revisions, including changes since an earlier revision", async () => {
    const { api } = fixture();
    const cr = (await api.change.listChanges({ repo })).changes.find((c) => c.title.startsWith("Rotate session"));
    const full = await api.change.getChangeDiff({ repo, changeRequestId: cr?.id ?? "", revision: 2 });
    expect(full.items.map((i) => pathText(i.path)).sort()).toEqual(["src/auth/session.ts", "tests/auth.test.ts"]);
    const since = await api.change.getChangeDiff({ repo, changeRequestId: cr?.id ?? "", revision: 2, sinceRevision: 1 });
    const hunk = since.items.find((i) => pathText(i.path) === "src/auth/session.ts")?.hunks[0];
    expect(hunk?.lines.some((l) => l.startsWith("+") && l.includes("rotateOnRoleChange"))).toBe(true);
  });

  it("merge queue lands the front entry over a few beats", async () => {
    const { api, store } = fixture();
    const q = await api.change.listQueue({ repo, ref: "main" });
    expect(q.entries[0]?.title).toBe("Document the webhook retry schedule");
    store.step();
    store.step();
    expect((await api.change.listQueue({ repo, ref: "main" })).entries).toHaveLength(0);
    const log = await api.query.log({ repo, from: { expr: "main" }, path: pathBytes("docs/webhooks.md") });
    expect(log.changesets[0]?.meta?.message).toBe("Document the webhook retry schedule");
  });
});

describe("fixture: leases", () => {
  it("rejects an overlapping exclusive lease with LEASED and returns advisory overlaps", async () => {
    const { api } = fixture();
    const err = await api.lease.acquireLease({ repo, ref: "main", paths: [pathBytes("src/auth/tokens.ts")], mode: "LEASE_MODE_ADVISORY" }).catch((e: unknown) => e);
    expect(err).toMatchObject({ reason: "LEASED" });
    const ok = await api.lease.acquireLease({ repo, ref: "main", paths: [pathBytes("src/billing")], mode: "LEASE_MODE_ADVISORY" });
    expect(ok.overlapping.length).toBeGreaterThanOrEqual(2);
  });
});

describe("fixture: code", () => {
  it("lists trees with restricted entries and refuses to read them", async () => {
    const { api } = fixture();
    const root = await api.query.listTree({ repo, at: { expr: "main" }, path: "" });
    expect(root.entries[0]?.mode).toBe("MODE_TREE");
    const ops = await api.query.listTree({ repo, at: { expr: "main" }, path: pathBytes("ops") });
    expect(ops.entries[0]?.restricted).toBe(true);
    await expect(api.readText({ repo, at: { expr: "main" }, path: pathBytes("ops/secrets.enc.yaml") })).rejects.toMatchObject({ reason: "PERMISSION_DENIED" });
  });

  it("blames lines to the changesets that introduced them", async () => {
    const { api } = fixture();
    const blame = await api.query.blame({ repo, at: { expr: "main" }, path: pathBytes("src/routes/health.ts") });
    const total = blame.ranges.reduce((n, r) => n + r.lineCount, 0);
    const text = await api.readText({ repo, at: { expr: "main" }, path: pathBytes("src/routes/health.ts") });
    expect(total).toBe(text.text.split("\n").length - 1);
    const ids = new Set(blame.ranges.map((r) => objectIdText(r.changeset)));
    expect(ids.size).toBeGreaterThanOrEqual(3);
  });

  it("resolves ref~N and changeset prefixes", async () => {
    const { api } = fixture();
    const log = await api.query.log({ repo, from: { expr: "main~2" } });
    expect(log.changesets[0]?.meta?.message).toBe("Add health route latency histogram");
    const id = objectIdText(log.changesets[1]?.id);
    const again = await api.query.log({ repo, from: { expr: id.slice(0, 14) } });
    expect(objectIdText(again.changesets[0]?.id)).toBe(id);
  });
});

describe("fixture: tokens and audit", () => {
  it("mints an attenuated token and refuses scope wider than the parent", async () => {
    const { api } = fixture();
    const agent = (await api.agent.listAgents({ org: "acme" })).agents.find((a) => a.principal.display === "triage");
    if (!agent) throw new Error("no agent");
    const scope = { repos: ["acme/api"], readPaths: [], writePaths: [pathBytes("tests")], refs: ["ws/*"], actions: ["read", "commit"], session: "sub", expiresAt: new Date(ANCHOR + 3_600_000).toISOString(), budget: { maxChangesets: "5", maxLanded: "0", maxUploadBytes: "0" } };
    const minted = await api.auth.mintToken({ principalId: agent.principal.id, scope });
    expect(minted.token).toMatch(/^swt_/);
    await expect(api.auth.mintToken({ principalId: agent.principal.id, scope: { ...scope, actions: ["admin"] } })).rejects.toMatchObject({ reason: "SCOPE_EXCEEDS_PARENT" });
    await expect(api.auth.mintToken({ principalId: agent.principal.id, scope: { ...scope, expiresAt: new Date(ANCHOR - 1).toISOString() } })).rejects.toMatchObject({ reason: "INVALID_EXPIRY" });
  });

  it("revoking a root token revokes every derived token and audits each", async () => {
    const { api } = fixture();
    const agent = (await api.agent.listAgents({ org: "acme" })).agents.find((a) => a.principal.display === "triage");
    const { tokens } = await api.agent.listTokens({ principalId: agent?.principal.id ?? "" });
    const root = tokens.find((t) => !t.parentTokenId);
    await api.auth.revoke({ tokenId: root?.tokenId ?? "" });
    const after = await api.agent.listTokens({ principalId: agent?.principal.id ?? "" });
    expect(after.tokens.every((t) => t.revoked)).toBe(true);
    const audit = await api.audit.listAudit({ org: "acme", action: "token.revoke" });
    expect(audit.entries.length).toBeGreaterThanOrEqual(3);
    expect(audit.chainVerified).toBe(true);
  });

  it("a revoked minted token can no longer authenticate", async () => {
    const { api, store } = fixture();
    const scope = { repos: [], readPaths: [], writePaths: [], refs: [], actions: ["read"], session: "", expiresAt: new Date(ANCHOR + 3_600_000).toISOString() };
    const minted = await api.auth.mintToken({ principalId: "", scope });
    const asAgent = createClient(fixtureRpc(store, minted.token));
    await expect(asAgent.auth.whoAmI({})).resolves.toBeTruthy();
    await api.auth.revoke({ tokenId: minted.tokenId });
    await expect(asAgent.auth.whoAmI({})).rejects.toMatchObject({ reason: "TOKEN_REVOKED" });
  });
});

describe("fixture: policy evaluate mode", () => {
  it("reports which recent changes a proposed policy would block", async () => {
    const { api } = fixture();
    const current = await api.policy.getPolicy({ repo });
    const stricter = current.source.replace('agents_may_land = ["generated", "docs", "comment_only", "test_only", "rename_only"]', 'agents_may_land = ["generated"]');
    const ev = await api.policy.evaluatePolicy({ repo, source: stricter, sample: 20 });
    expect(ev.valid).toBe(true);
    const blocked = ev.decisions.filter((d) => d.current && !d.proposed);
    expect(blocked.map((d) => d.title)).toContain("Document the webhook retry schedule");
  });

  it("returns line-numbered errors for invalid policy", async () => {
    const { api } = fixture();
    const ev = await api.policy.evaluatePolicy({ repo, source: 'version = 1\n[land."main"]\napprovals = [\n  { class = "risky", need = 1, from = "owner" },\n]\n', sample: 5 });
    expect(ev.valid).toBe(false);
    expect(ev.errors[0]).toMatchObject({ line: 3 });
  });
});

describe("fixture: live stream and logs", () => {
  it("Subscribe replays from a sequence, then delivers live events", async () => {
    const { api, store } = fixture();
    const last = Number(store.events.at(-1)?.seq);
    const ctl = new AbortController();
    const got: string[] = [];
    const done = (async () => {
      for await (const ev of api.event.subscribe({ repo, fromSeq: String(last - 2) }, { signal: ctl.signal })) {
        got.push(ev.kind);
        if (got.length === 3) ctl.abort();
      }
    })();
    await new Promise((r) => setTimeout(r, 10));
    store.step(); // docs-writer acquires a lease
    await done;
    expect(got).toHaveLength(3);
    expect(got[2]).toBe("lease.acquired");
  });

  it("running jobs print logs over time and finish, updating their check", async () => {
    let now = 0;
    const { api, store } = fixture(() => now);
    const run = (await api.workflow.listRuns({ repo, status: "running" })).runs[0];
    const job = run?.jobs.find((j) => j.status === "running");
    if (!run || !job) throw new Error("no running job");
    const early = store.jobLines(run, job.id).length;
    now += 1000;
    expect(store.jobLines(run, job.id).length).toBeGreaterThan(early);
    now += 60_000;
    const after = await api.workflow.getRun({ repo, id: run.id });
    expect(after.jobs.find((j) => j.id === job.id)?.status).toBe("passed");
  });

  it("rerunning a failed job fails again: reruns are a function of their inputs", async () => {
    let now = 0;
    const { api } = fixture(() => now);
    const failed = (await api.workflow.listRuns({ repo, status: "failed" })).runs[0];
    if (!failed) throw new Error("no failed run");
    const rerun = await api.workflow.rerun({ repo, id: failed.id, failedOnly: true });
    expect(rerun.attempt).toBe(2);
    expect(rerun.status).toBe("running");
    now += 60_000;
    expect((await api.workflow.getRun({ repo, id: failed.id })).status).toBe("failed");
  });
});

describe("fixture: imports and sign-in", () => {
  it("dry run estimates fidelity, mirror progresses to live", async () => {
    let now = 0;
    const { api } = fixture(() => now);
    const dry = await api.import.startImport({ source: "octo/widgets", org: false, targetOrg: "acme", phase: "IMPORT_PHASE_DRY_RUN" });
    now += 300;
    expect((await api.import.getImport({ id: dry.id })).state).toBe("IMPORT_STATE_LIVE");
    const report = await api.import.getFidelityReport({ id: dry.id });
    expect(report.items.find((i) => i.category === "review_comments")?.dropped).toBe("2");
    const mirror = await api.import.startImport({ source: "octo/widgets", org: false, targetOrg: "acme", phase: "IMPORT_PHASE_MIRROR" });
    now += 600;
    expect((await api.import.getImport({ id: mirror.id })).state).toBe("IMPORT_STATE_METADATA");
    now += 1000;
    expect((await api.import.getImport({ id: mirror.id })).state).toBe("IMPORT_STATE_LIVE");
    await expect(api.import.startImport({ source: "not a repo", org: false, targetOrg: "acme", phase: "IMPORT_PHASE_DRY_RUN" })).rejects.toMatchObject({ reason: "INVALID_SOURCE" });
  });

  it("device flow is pending until approved", async () => {
    const { api } = fixture();
    const start = await api.auth.login({ githubStart: true });
    await expect(api.auth.login({ githubDeviceCode: start.deviceCode })).rejects.toMatchObject({ reason: "AUTHORIZATION_PENDING", retryable: true });
    const done = await api.auth.login({ githubDeviceCode: start.deviceCode });
    expect(done.token).toMatch(/^swt_demo_/);
  });

  it("assigning an issue to an agent starts its work", async () => {
    const { api } = fixture();
    const agent = (await api.agent.listAgents({ org: "acme" })).agents.find((a) => a.principal.display === "docs-writer");
    const issue = await api.issue.assignIssue({ repo, number: 21, assigneeIds: [agent?.principal.id ?? ""] });
    expect(issue.comments.at(-1)?.author?.display).toBe("docs-writer");
    const project = await api.project.getProject({ repo });
    expect(project.columns.find((c) => c.id === "doing")?.issueNumbers[0]).toBe(21);
  });
});
