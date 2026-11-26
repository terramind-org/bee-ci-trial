import { describe, expect, it } from "vitest";
import { groupQueue, type QueueFacts } from "./queue";
import { brief, change, check, checklist, comment, landing, LIN, MARA, review } from "./brief-samples";

const NOW = Date.parse("2026-10-28T12:00:00Z");
const ok = [check("test", "CHECK_STATUS_PASSED")];

function facts(id: string, over: Partial<QueueFacts> & { cr?: Parameters<typeof change>[0] } = {}): QueueFacts {
  const { cr, ...rest } = over;
  return { change: change({ id, ...cr }), landing: landing(false, ["approval:human:logic (0 of 1)"]), checks: ok, reviews: [], ...rest };
}

describe("pull request list", () => {
  it("puts each pull request in the first group that takes it", () => {
    const groups = groupQueue(
      [
        facts("review-me"),
        facts("asks", { cr: { author: LIN, brief: brief({ asks: [{ question: "Ship behind a flag?", options: ["Yes", "No"] }] }) }, landing: landing(true) }),
        facts("ready", { landing: landing(true) }),
        facts("mine-ready", { cr: { author: MARA }, landing: landing(true) }),
        facts("running", { landing: landing(false, ["check:e2e (running)"]) }),
        facts("failing", { landing: landing(false, ["check:e2e (failed)"]), checks: [check("e2e", "CHECK_STATUS_FAILED")] }),
        facts("owes", { landing: landing(false, ["check:e2e (running)"]), checklist: checklist(false, [{ id: "evidence:ui", detail: "a screenshot of the UI change", kinds: [], paths: [], satisfied: false, evidenceIds: [] }]) }),
        facts("blocked", { cr: { author: MARA }, landing: landing(false, ["lease:exclusive:src/auth/"]) }),
        facts("landed", { cr: { state: "CHANGE_STATE_LANDED" }, landedAt: "2026-10-25T00:00:00Z" }),
        facts("old", { cr: { state: "CHANGE_STATE_LANDED" }, landedAt: "2026-10-01T00:00:00Z" }),
        facts("closed", { cr: { state: "CHANGE_STATE_CLOSED" } }),
      ],
      MARA,
      NOW,
    );
    const ids = Object.fromEntries(groups.map((g) => [g.key, g.rows.map((r) => r.facts.change.id)]));
    for (const k of Object.keys(ids)) ids[k]?.sort();
    // An agent's pull request that every rule allows waits for a person to merge it; the
    // viewer's own waits on nobody but them, and is not asked of them.
    expect(ids).toEqual({ "needs-you": ["asks", "ready", "review-me"], checks: ["owes", "running"], waiting: ["blocked", "failing", "mine-ready"], landed: ["landed"] });
    const row = (group: number, id: string) => groups[group]?.rows.find((r) => r.facts.change.id === id);
    expect(row(0, "asks")?.why).toBe("The author asks you to decide: Ship behind a flag?");
    expect(row(0, "review-me")?.why).toBe("Waiting for your review");
    expect(row(0, "ready")?.why).toBe("Every rule passes. It waits for a person to merge it");
    expect(row(1, "owes")?.why).toBe("The agent still owes a screenshot of the UI change");
    expect(row(1, "running")?.status.headline).toBe("Checks running");
    expect(row(2, "blocked")?.status.sentence).toBe("Blocked: it needs exclusive editing of src/auth/");
    expect(row(2, "failing")?.status.headline).toBe("Checks failing");
    expect(row(2, "mine-ready")?.why).toBe("Ready to merge: the one check passes");
  });

  it("does not ask the author, a past approver, or a signed-out viewer", () => {
    const answered = facts("answered", { cr: { author: LIN, brief: brief({ asks: [{ question: "Q?", options: ["A", "B"] }] }) }, comments: [comment(MARA, 'Decision on "Q?": A', "2026-10-27T10:00:00Z")], landing: landing(true) });
    const approved = facts("approved", { reviews: [review(MARA, "VERDICT_APPROVE", 2)] });
    const mine = facts("mine", { cr: { author: MARA } });
    const g = groupQueue([answered, approved, mine], MARA, NOW);
    expect(g[0]?.rows).toEqual([]);
    expect(g[2]?.rows.map((r) => r.facts.change.id).sort()).toEqual(["answered", "approved", "mine"]);
    // Signed out, nothing is asked of the reader: not a review, and not a merge.
    const out = groupQueue([facts("x"), facts("y", { landing: landing(true) })], null, NOW);
    expect(out[0]?.rows).toEqual([]);
    expect(out[2]?.rows).toHaveLength(2);
  });

  it("orders rows newest first and returns every group, even empty ones", () => {
    const a = facts("a", { landing: landing(true), cr: { revisions: [{ n: 1, diffClass: "logic", createdAt: "2026-10-27T01:00:00Z" }] } });
    const b = facts("b", { landing: landing(true), cr: { revisions: [{ n: 1, diffClass: "logic", createdAt: "2026-10-27T02:00:00Z" }] } });
    const g = groupQueue([a, b], MARA, NOW);
    expect(g.map((x) => x.key)).toEqual(["needs-you", "checks", "waiting", "landed"]);
    expect(g.map((x) => x.label)).toEqual(["Needs you", "Checks running", "Waiting on someone else", "Merged this week"]);
    expect(g[0]?.rows.map((r) => r.facts.change.id)).toEqual(["b", "a"]);
  });
});
