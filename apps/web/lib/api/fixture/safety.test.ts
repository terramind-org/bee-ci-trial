import { describe, expect, it } from "vitest";
import { toCli, toCurl, toMcp } from "../../copyas";
import { createClient } from "../client";
import { wireOf } from "../ops";
import type { SafetyFindingReport, SafetyRef } from "../types";
import { FixtureStore, fixtureRpc } from "./index";

const repo = { fullName: "acme/api" };
const ANCHOR = Date.UTC(2026, 9, 18, 9);
const report = {
  kind: "SAFETY_SIGNAL_KIND_LEGAL_FLAGS",
  severity: "SAFETY_SEVERITY_WARNING",
  evidence: { observation: "The requested export needs a retention review." },
} satisfies SafetyFindingReport;

function world() {
  const store = new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false });
  const mara = createClient(fixtureRpc(store, "mara-token-0001"));
  const solo = store.swarms.runs.find((r) => r.trigger === "manual")!;
  const work = { repo, kind: "SAFETY_WORK_KIND_RUN", id: solo.id! } satisfies SafetyRef;
  const as = (handle: string, session = "") => {
    const token = "safety-token-" + handle + session;
    const tokenId = "safety-test-" + handle + session;
    store.tokens.push({
      tokenId, parentTokenId: "", principalId: store.p(handle).id,
      scope: { repos: [], refs: [], readPaths: [], writePaths: [], actions: [], session },
      revoked: false, label: "Safety test",
    });
    store.tokenSecrets.set(token, tokenId);
    return createClient(fixtureRpc(store, token));
  };
  const period = { start: store.iso(store.ago(5_000)), end: store.iso(store.now() + 1) };
  return { store, mara, solo, work, as, period };
}

describe("safety operations against the demo repository", () => {
  it("loads the eight records with all verdicts, sourced asks and paths backed by the fixture changes", async () => {
    const { store, mara, as } = world();
    const first = await mara.safety.listSafetyRecords({ repo, page: { pageSize: 4 } });
    const next = await mara.safety.listSafetyRecords({ repo, page: { pageSize: 4, pageToken: first.page?.nextPageToken } });
    const records = [...first.records, ...next.records];
    expect(first.page?.nextPageToken).toBeTruthy();
    expect(next.page?.nextPageToken).toBe("");
    expect(records).toHaveLength(8);
    expect(new Set(records.map((r) => r.verdict))).toEqual(new Set([
      "SAFETY_VERDICT_AS_ASKED", "SAFETY_VERDICT_MORE_THAN_ASKED", "SAFETY_VERDICT_LESS_THAN_ASKED", "SAFETY_VERDICT_SOMETHING_ELSE", "SAFETY_VERDICT_NEEDS_A_LOOK",
    ]));
    const landed = store.crByKey("histogram");
    const health = records.find((r) => r.work?.id === store.swarms.swarms[1]?.id);
    expect(health).toMatchObject({
      asked: expect.arrayContaining([{ text: expect.stringContaining("p99"), source: { kind: "SUBJECT_KIND_SWARM", repo: { id: store.repo(repo).repo.id }, id: health?.work?.id } }]),
      done: { paths: ["src/routes/health.ts"], commands: ["bun test"], mergedChangeRequestIds: [landed.id] },
    });
    const dismissed = records.flatMap((r) => r.findings).find((f) => f.state === "SAFETY_FINDING_STATE_DISMISSED");
    expect(dismissed).toMatchObject({
      resolutionReason: expect.stringContaining("Revision 2"),
      report: { evidence: { paths: ["src/auth/session.ts"], subjects: [{ id: store.crByKey("rotate").id }] } },
    });
    await expect(as("rhea").safety.listSafetyRecords({ repo })).rejects.toMatchObject({ code: "not_found" });
  });

  it("sends new attention to the answerable person and keeps concerns out of findings", async () => {
    const { store, mara, as, solo, work } = world();
    const attention = (await mara.safety.listSafetyAttention({ repo })).items;
    expect(attention).toHaveLength(2);
    expect(attention.find((a) => a.tripwire)?.tripwire?.reason).toContain("customer identifiers");
    expect(attention.find((a) => a.concern)?.concern?.text).toContain("redacted session");
    expect((await as("sam").safety.listSafetyAttention({ repo })).items).toEqual([]);
    await expect(as("sam").safety.acknowledgeSafetyAttention({ repo, id: attention[0]!.id })).rejects.toMatchObject({ code: "not_found" });

    const before = await mara.safety.getSafetyRecord(work);
    const input = { repo, runId: solo.id!, category: "SAFETY_CONCERN_CATEGORY_UNSAFE", text: "The request needs a redacted sample." } as const;
    await expect(as("watcher").safety.raiseConcern(input)).rejects.toMatchObject({ code: "permission_denied" });
    const concern = await as("watcher", solo.session).safety.raiseConcern(input);
    expect(concern).toMatchObject({ text: input.text, runId: solo.id });
    expect(await mara.agent.getAgentRun({ id: solo.id! })).toMatchObject({ state: "AGENT_RUN_STATE_CANCELLED" });
    const after = await mara.safety.getSafetyRecord(work);
    expect(after.findings).toEqual(before.findings);
    expect(after.verdict).toBe(before.verdict);
    const item = (await mara.safety.listSafetyAttention({ repo })).items.find((n) => n.concern?.id === concern.id);
    expect(item).toMatchObject({ answerable: { id: store.p("mara").id }, work: { id: solo.id }, concern });
    expect((await mara.safety.acknowledgeSafetyAttention({ repo, id: item!.id })).acknowledgedAt).toBeTruthy();
    expect((await mara.safety.listSafetyAttention({ repo })).items).toHaveLength(2);
  });

  it("keeps the independent agent's proposed verdict separate from the person's", async () => {
    const { store, mara, as } = world();
    const health = store.swarms.swarms[1]!;
    const work = { repo, kind: "SAFETY_WORK_KIND_SWARM", id: health.id } satisfies SafetyRef;
    await expect(mara.safety.requestSafetyReview({ work, reviewerId: store.p("watcher").id })).rejects.toMatchObject({ code: "permission_denied" });
    const reviewer = as("reviewer");
    const review = await mara.safety.requestSafetyReview({ work, reviewerId: store.p("reviewer").id });
    const finding = await reviewer.safety.reportFinding({ work, reviewId: review.id, report });
    const entry = await reviewer.safety.submitSafetyReview({
      work, reviewId: review.id, model: "independent-review", proposedVerdict: "SAFETY_VERDICT_SOMETHING_ELSE", findings: [report],
    });
    expect(entry).toMatchObject({ findingIds: [finding.id, expect.any(String)], model: "independent-review", proposedVerdict: "SAFETY_VERDICT_SOMETHING_ELSE" });
    expect((await mara.safety.getSafetyRecord(work)).verdict).toBe("SAFETY_VERDICT_NEEDS_A_LOOK");

    const result = await mara.safety.resolveFinding({ work, findingId: finding.id, state: "SAFETY_FINDING_STATE_CONFIRMED", reason: "The retention note is still missing." });
    expect(result.resolvedBy?.id).toBe(store.p("mara").id);
    const verdict = await mara.safety.setVerdict({ work, verdict: "SAFETY_VERDICT_LESS_THAN_ASKED", note: "The requested retention steps are incomplete." });
    expect(verdict.override).toMatchObject({ verdict: verdict.verdict, person: { id: store.p("mara").id } });
    expect(store.events.map((e) => e.kind)).toEqual(expect.arrayContaining(["safety.review_requested", "safety.review_submitted"]));
    expect(store.audit.some((e) => e.action === "safety.verdict_set")).toBe(true);
  });

  it("trips a severe report and pauses the worked-on swarm", async () => {
    const { store, mara } = world();
    const working = store.swarms.swarms[0]!;
    const work = { repo, kind: "SAFETY_WORK_KIND_SWARM", id: working.id } satisfies SafetyRef;
    const finding = await mara.safety.reportFinding({ work, report: { ...report, kind: "SAFETY_SIGNAL_KIND_OWN_LEASH", severity: "SAFETY_SEVERITY_SEVERE" } });
    expect((await mara.swarm.getSwarm({ repo, id: working.id })).state).toBe("SWARM_STATE_PAUSED");
    const items = (await mara.safety.listSafetyAttention({ repo })).items;
    expect(items[0]).toMatchObject({ work: { id: working.id }, tripwire: { findingId: finding.id, pausedSwarm: true, stoppedRunIds: expect.arrayContaining([working.leadRunId]) } });
  });

  it("freezes agent merges for the owner and lifts the freeze without reviving runs", async () => {
    const { store, mara, as, solo } = world();
    const request = { org: "acme", reason: "Pause while investigating the report." };
    await expect(as("dev").safety.stopEverything(request)).rejects.toMatchObject({ code: "permission_denied" });
    const stopped = await mara.safety.stopEverything(request);
    expect(stopped).toMatchObject({ frozen: true, stoppedRunIds: expect.arrayContaining([solo.id]), pausedSwarmIds: [store.swarms.swarms[0]?.id] });
    const changeRequestId = store.crByKey("rotate").id;
    await expect(as("triage").write.land({ repo, ref: "main", changeRequestId })).rejects.toMatchObject({ detail: { subcode: "safety_merge_frozen" } });
    expect((await mara.safety.resumeEverything({ ...request, reason: "The report was reviewed." })).frozen).toBe(false);
    expect(store.safety.frozen.has("acme")).toBe(false);
    expect((await mara.agent.getAgentRun({ id: solo.id! })).state).toBe("AGENT_RUN_STATE_CANCELLED");
  });

  it("samples actual agent merges reproducibly and summarizes signals for the period", async () => {
    const { mara, as, period, store } = world();
    const req = { repo, period, seed: "review-day", limit: 2 };
    const landed = store.crByKey("histogram");
    expect((await mara.safety.spotCheck(req)).records).toEqual([]);
    // An agent's approval does not disqualify the actual agent merge from the sample.
    landed.reviews = landed.reviews.map((review) => ({ ...review, reviewer: store.p("reviewer") }));
    const sample = await mara.safety.spotCheck(req);
    expect(sample.records).toMatchObject([{ work: { id: store.swarms.swarms[1]?.id }, done: { mergedChangeRequestIds: [landed.id] } }]);
    expect(await mara.safety.spotCheck(req)).toEqual(sample);
    const summary = await mara.safety.getSafetySummary({ org: "acme", period });
    expect(summary.signals).toEqual(expect.arrayContaining([
      { kind: "SAFETY_SIGNAL_KIND_HARM_TO_OTHERS", count: "1" }, { kind: "SAFETY_SIGNAL_KIND_OUTSIDE_TASK", count: "1" },
    ]));
    expect(summary.work.some((w) => w.kind === "SAFETY_WORK_KIND_SWARM" && w.id === store.swarms.swarms[0]?.id)).toBe(true);
    expect(summary.tripwires).toHaveLength(1);
    expect(Number(summary.medianWaitMs)).toBeGreaterThan(0);
    await expect(as("sam").safety.getSafetySummary({ org: "acme", period })).rejects.toMatchObject({ code: "permission_denied" });
  });

  it("copies the concern as a Bee command and the generated MCP tool", () => {
    const request = { repo, runId: "01S", category: "SAFETY_CONCERN_CATEGORY_RULE_CONFLICT", text: "The task conflicts with the rule." };
    const op = { service: "SafetyService", method: "RaiseConcern", request } as const;
    expect(wireOf(op.service, op.method, request)).toMatchObject({ ...op, streaming: false });
    expect(toCli(op)).toContain("bee safety raise-concern");
    expect(toCurl(op)).toContain("/bee.v1.SafetyService/RaiseConcern");
    expect(JSON.parse(toMcp(op)).params).toEqual({ name: "safety_raise_concern", arguments: request });
  });
});
