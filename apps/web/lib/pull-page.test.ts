import { describe, expect, it } from "vitest";
import { textToBase64 } from "./api/ids";
import type { AgentSession, Provenance } from "./api/types";
import { AGENT, brief, change, check, comment, diffItem, landing, LIN, MARA, review } from "./brief-samples";
import { decisionsFor, pullFacts, whoDidIt } from "./pull-page";
import { statusOf } from "./status";

const session = (over: Partial<AgentSession> = {}): AgentSession => ({ session: "s1", runId: "", agentPrincipalId: AGENT.id, model: "claude-opus-5", runtime: "claude-code/3.2", promptDigest: textToBase64("b8152e3ad0ed5346"), chain: [], changesets: [], changeRequestIds: [], cost: { inputTokens: "10", outputTokens: "5", usdMicros: "870000" }, createdAt: "2026-10-27T10:00:00Z", ...over });
const provenance: Provenance = { principalChain: [AGENT.id, MARA.id], runtime: "claude-code/3.2", model: "claude-opus-5", session: "s1", promptDigest: textToBase64("b8152e3ad0ed5346"), checksRun: [{ name: "bun test", result: "passed", durationMs: "7000" }], cost: { inputTokens: "1", outputTokens: "1", usdMicros: "20000" } };

describe("decisions on a pull request", () => {
  const open = change({ author: AGENT });

  it("lets a person who did not write it approve and merge in one press", () => {
    const d = decisionsFor({ change: open, viewer: LIN, reviews: [], landing: landing(false, ["approval:human:sensitive (1 of 2)"]), queue: null });
    expect(d).toMatchObject({ primary: "approve-and-merge", canRequestChanges: true, canClose: true });
    expect(d.note).toBe("Your approval is recorded first; it merges if the rules then allow it.");
  });

  it("offers Merge once they approved the latest revision and the rules allow it", () => {
    const latest = open.revisions.at(-1)?.n ?? 0;
    expect(decisionsFor({ change: open, viewer: LIN, reviews: [review(LIN, "VERDICT_APPROVE", latest)], landing: landing(true), queue: null }).primary).toBe("merge");
    const waiting = decisionsFor({ change: open, viewer: LIN, reviews: [review(LIN, "VERDICT_APPROVE", latest)], landing: landing(false, ["check:test (running)"]), queue: null });
    expect(waiting.primary).toBeNull();
    expect(waiting.note).toContain("still waits on something else");
    // An approval of an earlier revision does not count for this one.
    expect(decisionsFor({ change: open, viewer: LIN, reviews: [review(LIN, "VERDICT_APPROVE", latest - 1)], landing: landing(false), queue: null }).primary).toBe("approve-and-merge");
  });

  it("never lets an author approve their own, and nobody decide on a merged, closed or queued one", () => {
    const mine = change({ author: MARA });
    expect(decisionsFor({ change: mine, viewer: MARA, reviews: [], landing: landing(false), queue: null })).toMatchObject({ primary: null, canRequestChanges: false, canClose: true });
    expect(decisionsFor({ change: mine, viewer: MARA, reviews: [], landing: landing(true), queue: null }).primary).toBe("merge");
    expect(decisionsFor({ change: change({ state: "CHANGE_STATE_LANDED" }), viewer: LIN, reviews: [] })).toMatchObject({ primary: null, canRequestChanges: false, canClose: false });
    expect(decisionsFor({ change: change({ state: "CHANGE_STATE_CLOSED" }), viewer: LIN, reviews: [] }).canClose).toBe(false);
    expect(decisionsFor({ change: open, viewer: LIN, reviews: [], landing: landing(true), queue: { position: 2 } })).toMatchObject({ primary: null, canClose: true });
    expect(decisionsFor({ change: open, viewer: null, reviews: [], landing: landing(true) })).toMatchObject({ primary: null, canRequestChanges: false, canClose: false, note: "Sign in to approve it, request changes or close it." });
  });
});

describe("who did it", () => {
  it("reads the agent, the person, the model, the cost and the prompt digest from the sessions and the head", () => {
    const who = whoDidIt({ change: change({ author: AGENT }), answers: MARA, sessions: [session(), session({ session: "s2", cost: { inputTokens: "1", outputTokens: "1", usdMicros: "130000" } })], provenance });
    expect(who).toMatchObject({ author: "triage", agent: true, agentId: AGENT.id, person: "Mara Okafor", models: ["claude-opus-5"], runtimes: ["Claude Code"], costMicros: 1_000_000, sessions: 2, promptDigests: ["b8152e3ad0ed5346"], ownChecks: ["bun test passed"] });
  });

  it("leaves every line empty that the data does not have", () => {
    const who = whoDidIt({ change: change({ author: LIN }), answers: LIN });
    expect(who).toMatchObject({ author: "Lin Chen", agent: false, person: "Lin Chen", models: [], runtimes: [], costMicros: 0, sessions: 0, promptDigests: [], ownChecks: [] });
    // With no sessions the head's own cost is all there is; with no owner known there is no person.
    expect(whoDidIt({ change: change({ author: AGENT }), provenance }).costMicros).toBe(20_000);
    expect(whoDidIt({ change: change({ author: AGENT }), provenance }).person).toBeUndefined();
  });
});

describe("the pull request for an agent", () => {
  const cr = change({ author: AGENT, brief: brief({ asks: [{ question: "Ship behind a flag?", options: ["Yes", "No"] }] }) });
  const land = landing(false, ["approval:human:sensitive (1 of 2)", "check:e2e (running)"], ["check:lint"]);
  const checks = [check("lint", "CHECK_STATUS_PASSED"), check("e2e", "CHECK_STATUS_RUNNING")];
  const who = whoDidIt({ change: cr, answers: MARA, sessions: [session()], provenance });
  const groups = pullFacts({
    repo: "acme/api",
    change: cr,
    status: statusOf({ change: cr, landing: land, checks, reviews: [], queue: null }),
    branch: "ws/triage/rotate-sessions",
    answers: MARA,
    commits: { known: true, lines: ["bbd8c2e469 · Rotate session ids · triage for Mara Okafor"] },
    landing: land,
    checks,
    evidence: [],
    diff: [diffItem("src/auth/session.ts", "sensitive", ["-old", "+new"]), diffItem("docs/a.md", "docs", ["+d"])],
    reviews: [review(LIN, "VERDICT_APPROVE", 1)],
    comments: [comment(MARA, "Looks right.\nSecond line", "2026-10-27T12:00:00Z")],
    who,
    decisions: decisionsFor({ change: cr, viewer: null, reviews: [], landing: land }),
    href: "/acme/api/changes/cr-1",
    changeMdHref: "/acme/api/changes/cr-1/change.md",
  });
  const group = (title: string) => groups.find((g) => g.title === title);
  const facts = (title: string) => Object.fromEntries((group(title)?.facts ?? []).flatMap((f) => (f ? [[f.label, f.value]] : [])));

  it("states the same facts the person's page shows, in the page's order", () => {
    expect(groups.map((g) => g.title)).toEqual(["facts", "why a person", "checks", "files changed", "commits", "who did it", "conversation"]);
    expect(facts("facts")).toMatchObject({ "pull request": "acme/api #" + cr.number, title: cr.title, author: "triage (agent)", "answers for it": "Mara Okafor", merge: "triage wants to merge one commit into main from ws/triage/rotate-sessions", "full text": "/acme/api/changes/cr-1/change.md" });
  });

  it("gives the rule in words and as the server's own token", () => {
    const why = group("why a person")?.lines ?? [];
    expect(why).toContain("A change to a path the rules mark sensitive needs two approvals from a person. It has one.");
    expect(why).toContain("It is waiting for the e2e check.");
    expect(why).toContain("missing: approval:human:sensitive (1 of 2)");
    expect(why).toContain("met: check:lint");
    expect(why).toContain("asked: Ship behind a flag? (Yes / No) · not decided");
  });

  it("lists checks, files, commits and who did it, prompt digest included", () => {
    expect(group("checks")?.lines).toEqual(expect.arrayContaining(["lint: passed", "e2e: running"]));
    expect(group("files changed")?.lines?.some((l) => l.startsWith("worth a look: src/auth/session.ts"))).toBe(true);
    expect(group("files changed")?.lines).toContain("docs/a.md · +1 −0 · docs");
    expect(group("commits")?.lines).toHaveLength(1);
    expect(facts("who did it")).toMatchObject({ agent: "triage", for: "Mara Okafor", model: "claude-opus-5", cost: "$0.87", "prompt digest": "b8152e3ad0ed5346", "checks it ran itself": "bun test passed" });
    expect(group("conversation")?.lines).toEqual(["Lin Chen approved r1", "Mara Okafor: Looks right."]);
  });

  it("drops the rules for a merged pull request and never invents a commit count", () => {
    const merged = change({ author: AGENT, state: "CHANGE_STATE_LANDED" });
    const out = pullFacts({ repo: "acme/api", change: merged, status: statusOf({ change: merged, checks: [], reviews: [] }), branch: "", commits: { known: false, lines: [] }, checks: [], evidence: [], diff: [], reviews: [], comments: [], who: whoDidIt({ change: merged }), decisions: decisionsFor({ change: merged, viewer: null, reviews: [] }), href: "/x", changeMdHref: "/x/change.md" });
    expect(out.map((g) => g.title)).not.toContain("why a person");
    expect(out[0]?.facts?.find((f) => f?.label === "merge")?.value).toBe("triage merged into main");
    expect(out.find((g) => g.title === "commits")?.empty).toBe("The server did not list its commits.");
  });
});
