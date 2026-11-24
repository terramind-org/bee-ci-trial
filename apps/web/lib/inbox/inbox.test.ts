import { describe, expect, it } from "vitest";
import type { AgentSummary, ChangeRequest, CheckRun, FeedEvent, LandingEvaluation, Post, Principal, Review } from "@/lib/api/types";
import { classify, needsOf, postNeed, sortNeeds, type ChangeFacts } from "./needs";
import { DAY, agentWork, doneSince, landedTitle, progressOf, weekOf } from "./progress";
import { checksInWords, missingInWords, parseApproval, riskOf, runtimeName } from "./words";

const me: Principal = { id: "me", kind: "PRINCIPAL_KIND_HUMAN", display: "Mara Okafor", ownerId: "", runtime: "", externalIds: [] };
const bot: Principal = { id: "bot", kind: "PRINCIPAL_KIND_AGENT", display: "triage", ownerId: "me", runtime: "claude-code", externalIds: [] };
const dev: Principal = { id: "dev", kind: "PRINCIPAL_KIND_HUMAN", display: "Dev Patel", ownerId: "", runtime: "", externalIds: [] };

function change(author: Principal, revisions = 1): ChangeRequest {
  return {
    id: "cr-" + author.id,
    repoId: "r",
    targetRef: "main",
    changeIds: [],
    state: "CHANGE_STATE_OPEN",
    author,
    title: "Rotate session ids",
    body: "",
    revisions: Array.from({ length: revisions }, (_, i) => ({ n: i + 1, diffClass: "sensitive", createdAt: `2026-10-2${i}T00:00:00Z` })),
    dependsOn: "",
    number: "101",
  };
}
const check = (name: string, status: CheckRun["status"]): CheckRun => ({ id: name, repoId: "r", name, status, url: "", summary: "", postLand: false });
const landing = (missing: string[], allowed = false): LandingEvaluation => ({ allowed, missing, satisfied: [], diffClass: "sensitive" });
const review = (reviewer: Principal, verdict: Review["verdict"], revision: number): Review => ({ id: reviewer.id + revision, changeId: "c", revision, reviewer, verdict, body: "", comments: [] });

function facts(over: Partial<ChangeFacts>): ChangeFacts {
  return { repo: "acme/api", change: change(bot), landing: landing([]), checks: [check("test", "CHECK_STATUS_PASSED")], reviews: [], owners: {}, queued: false, ...over };
}

describe("words", () => {
  it("parses and phrases approval rules", () => {
    expect(parseApproval("approval:human:sensitive (1 of 2)")).toEqual({ from: "human", scope: "sensitive", have: 1, need: 2 });
    expect(parseApproval("approval:any (0 of 1)")).toEqual({ from: "any", scope: "", have: 0, need: 1 });
    expect(missingInWords("approval:human:sensitive (1 of 2)")).toBe("needs one more approval from a person");
    expect(missingInWords("approval:owner:src/billing (0 of 1)")).toBe("needs one approval from an owner of src/billing");
    expect(missingInWords("check:test (failed)")).toBe("failed the test check");
    expect(missingInWords("stack: parent Fix rounding must land first")).toBe("waits for “Fix rounding” to merge first");
  });

  it("names runtimes and risk in plain words", () => {
    expect(runtimeName("claude-code/3.2")).toBe("Claude Code");
    expect(runtimeName("codex")).toBe("Codex");
    expect(runtimeName("my-runner")).toBe("My Runner");
    expect(riskOf("sensitive")).toMatchObject({ level: "High", step: 8, steps: 8 });
    expect(riskOf("docs").level).toBe("Very low");
  });

  it("summarises checks", () => {
    expect(checksInWords([check("test", "CHECK_STATUS_PASSED"), check("lint", "CHECK_STATUS_PASSED")])).toBe("All two checks passed.");
    expect(checksInWords([check("test", "CHECK_STATUS_FAILED"), check("lint", "CHECK_STATUS_PASSED"), check("e2e", "CHECK_STATUS_RUNNING")])).toBe("1 of 3 checks passed; test failed, e2e is still running.");
  });
});

describe("classify", () => {
  it("asks for a review the caller can give, and notes a revision since their request", () => {
    const n = classify(facts({ change: change(bot, 3), landing: landing(["approval:human:sensitive (1 of 2)"]), reviews: [review(me, "VERDICT_REQUEST_CHANGES", 1), review(dev, "VERDICT_APPROVE", 2)] }), me);
    expect(n).toMatchObject({ kind: "review", revisedAfterYou: true });
  });

  it("ignores optional checks but waits for required ones", () => {
    const optional = facts({ landing: landing(["approval:any (0 of 1)"]), checks: [check("e2e", "CHECK_STATUS_RUNNING")] });
    expect(classify(optional, me)?.kind).toBe("review");
    expect(classify(facts({ landing: landing(["check:test (running)", "approval:any (0 of 1)"]) }), me)).toBeNull();
  });

  it("leaves owner approvals to the owners", () => {
    const f = facts({ landing: landing(["approval:owner:src/billing (0 of 1)"]), owners: { "src/billing": ["Dev Patel"] } });
    expect(classify(f, me)).toBeNull();
    expect(classify({ ...f, owners: { "src/billing": ["Mara Okafor"] } }, me)?.kind).toBe("review");
  });

  it("does not ask the caller to approve their own change or one they approved", () => {
    expect(classify(facts({ change: change(me), landing: landing(["approval:human:sensitive (0 of 2)"]) }), me)).toBeNull();
    expect(classify(facts({ landing: landing(["approval:any (0 of 1)"]), reviews: [review(me, "VERDICT_APPROVE", 1)] }), me)).toBeNull();
  });

  it("surfaces failing agents, finished agents and policy blocks", () => {
    expect(classify(facts({ checks: [check("test", "CHECK_STATUS_FAILED")] }), me)).toMatchObject({ kind: "failing", failing: ["test"] });
    expect(classify(facts({ change: change(dev), checks: [check("test", "CHECK_STATUS_FAILED")] }), me)).toBeNull();
    expect(classify(facts({ landing: landing([], true) }), me)?.kind).toBe("merge");
    expect(classify(facts({ landing: landing([], true), queued: true }), me)).toBeNull();
    expect(classify(facts({ landing: landing(["stack: parent X must land first"]) }), me)).toMatchObject({ kind: "blocked", reasons: ["waits for “X” to merge first"] });
  });

  it("orders failing work first", () => {
    const review1 = classify(facts({ landing: landing(["approval:any (0 of 1)"]) }), me)!;
    const failing = classify(facts({ checks: [check("test", "CHECK_STATUS_FAILED")] }), me)!;
    expect(sortNeeds([review1, failing]).map((n) => n.kind)).toEqual(["failing", "review"]);
  });
});

describe("progress", () => {
  it("places work on one of four steps", () => {
    expect(progressOf(undefined, true)).toMatchObject({ step: 0, label: "Editing" });
    expect(progressOf(undefined, false)).toBeNull();
    expect(progressOf(facts({ landing: landing(["check:test (running)"]) }), false)).toMatchObject({ step: 1, tone: "calm" });
    expect(progressOf(facts({ checks: [check("test", "CHECK_STATUS_FAILED")] }), false)).toMatchObject({ step: 1, tone: "stuck" });
    expect(progressOf(facts({ landing: landing(["approval:any (0 of 1)"]) }), false)).toMatchObject({ step: 2 });
    expect(progressOf(facts({ queued: true }), false)).toMatchObject({ step: 3, label: "In the merge queue" });
  });

  it("lists active agents with work, stuck ones first", () => {
    const summary = (p: Principal, status: AgentSummary["status"] = "active"): AgentSummary => ({
      principal: p,
      purpose: "",
      status,
      model: "",
      session: "",
      openChanges: 0,
      landed7d: 0,
      spend7dUsdMicros: "0",
      activeLeases: 0,
      registered: true,
    });
    const other: Principal = { ...bot, id: "bot2", display: "deps-bot" };
    const rows = agentWork(
      [summary(bot), summary(other), summary({ ...bot, id: "idle", display: "idle" }, "idle")],
      [{ repo: "acme/api", lease: { id: "l", repoId: "r", ref: "main", paths: [], holder: bot, mode: "LEASE_MODE_ADVISORY", note: "x" } }],
      [facts({ change: change(other), checks: [check("test", "CHECK_STATUS_FAILED")] })],
    );
    expect(rows.map((r) => [r.agent.principal.display, r.progress?.label])).toEqual([
      ["deps-bot", "Stuck on a check"],
      ["triage", "Editing"],
    ]);
  });

  it("groups merges in the caller's repos since a time", () => {
    const ev = (id: string, repo: string, at: string, kind = "land"): FeedEvent => ({
      id,
      kind,
      summary: `x landed "T${id}" in ${repo}`,
      payload: "",
      createdAt: at,
      subject: { kind: "SUBJECT_KIND_CHANGESET", id, repo: { fullName: repo } },
    });
    const groups = doneSince(
      [
        ev("1", "acme/api", "2026-10-27T10:00:00Z"),
        ev("2", "acme/api", "2026-10-27T11:00:00Z"),
        ev("3", "acme/sdk", "2026-10-27T12:00:00Z"),
        ev("4", "other/x", "2026-10-27T12:00:00Z"),
        ev("5", "acme/api", "2026-10-20T12:00:00Z"),
        ev("6", "acme/api", "2026-10-27T12:00:00Z", "star"),
      ],
      new Set(["acme/api", "acme/sdk"]),
      Date.parse("2026-10-27T00:00:00Z"),
    );
    expect(groups.map((g) => [g.repo, g.items.length])).toEqual([
      ["acme/api", 2],
      ["acme/sdk", 1],
    ]);
    expect(landedTitle(groups[0]!.items[0]!)).toBe("T1");
  });
});

describe("posts waiting for approval", () => {
  const post = (over: Partial<Post> = {}, canApprove = true): Post => ({
    id: "p1",
    repo: { fullName: "acme/api" },
    changeId: "cr-bot",
    changeNumber: "101",
    changeTitle: "Rotate session ids",
    author: bot,
    text: "Session ids rotate now.",
    media: [],
    state: "POST_STATE_PENDING_APPROVAL",
    createdAt: "2026-10-27T09:00:00Z",
    updatedAt: "2026-10-27T10:00:00Z",
    version: "1",
    withdrawnReason: "",
    viewer: { liked: false, reposted: false, canEdit: false, canApprove, canWithdraw: false },
    ...over,
  });
  const landed = (p?: Post): ChangeRequest => ({ ...change(bot), state: "CHANGE_STATE_LANDED", post: p });

  it("asks only when the server says this person may approve a merged change's pending post", () => {
    expect(postNeed("acme/api", landed(post()), me)).toMatchObject({ kind: "post", who: bot, at: "2026-10-27T10:00:00Z" });
    expect(postNeed("acme/api", landed(post({}, false)), me)).toBeNull();
    expect(postNeed("acme/api", landed(post({ state: "POST_STATE_DRAFT" })), me)).toBeNull();
    expect(postNeed("acme/api", landed(post({ state: "POST_STATE_PUBLISHED" })), me)).toBeNull();
    expect(postNeed("acme/api", landed(post({ author: me })), me)).toBeNull();
    expect(postNeed("acme/api", landed(), me)).toBeNull();
  });

  it("lists changes and posts together, posts last", () => {
    const needs = needsOf(
      [facts({ landing: landing(["approval:any (0 of 1)"]) }), facts({ change: change(dev), landing: landing(["approval:owner:x (0 of 1)"]) })],
      [{ repo: "acme/api", change: landed(post()) }, { repo: "acme/api", change: landed(post({ state: "POST_STATE_PUBLISHED" })) }],
      me,
    );
    expect(needs.map((n) => n.kind)).toEqual(["review", "post"]);
  });
});

describe("this week", () => {
  it("counts merges in the caller's repos over seven days, agent spend and open requests", () => {
    const now = Date.parse("2026-10-28T12:00:00Z");
    const ev = (id: string, at: number, actor: Principal, repo = "acme/api"): FeedEvent => ({
      id,
      kind: "land",
      actor,
      summary: "",
      payload: "",
      createdAt: new Date(at).toISOString(),
      subject: { kind: "SUBJECT_KIND_CHANGESET", id, repo: { fullName: repo } },
    });
    const agent = (spend: string): AgentSummary => ({ principal: bot, purpose: "", status: "active", model: "", session: "", openChanges: 0, landed7d: 0, spend7dUsdMicros: spend, activeLeases: 0, registered: true });
    const week = weekOf([ev("1", now - DAY, bot), ev("2", now - 2 * DAY, me), ev("3", now - 8 * DAY, bot), ev("4", now - DAY, bot, "other/x")], new Set(["acme/api"]), now, [agent("1500000"), agent("250000")], 4);
    expect(week).toEqual({ merged: 2, byAgents: 1, open: 4, spendMicros: 1_750_000 });
  });
});
