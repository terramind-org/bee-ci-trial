import { describe, expect, it } from "vitest";
import { groupQueue, type QueueFacts } from "./queue";
import { brief, change, check, checklist, comment, landing, LIN, MARA, review } from "./brief-samples";

const NOW = Date.parse("2026-10-28T12:00:00Z");
const ok = [check("test", "CHECK_STATUS_PASSED")];

function facts(id: string, over: Partial<QueueFacts> & { cr?: Parameters<typeof change>[0] } = {}): QueueFacts {
  const { cr, ...rest } = over;
  return { change: change({ id, ...cr }), landing: landing(false, ["approval:human:logic (0 of 1)"]), checks: ok, reviews: [], ...rest };
}

describe("review queue", () => {
  it("puts each change in the first group that takes it", () => {
    const groups = groupQueue(
      [
        facts("review-me"),
        facts("asks", { cr: { author: LIN, brief: brief({ asks: [{ question: "Ship behind a flag?", options: ["Yes", "No"] }] }) }, landing: landing(true) }),
        facts("ready", { landing: landing(true) }),
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
    expect(ids).toEqual({ "needs-you": ["asks", "review-me"].sort(), ready: ["ready"], waiting: ["owes"], blocked: ["blocked"], landed: ["landed"] });
    const needs = groups[0]?.rows ?? [];
    expect(needs.find((r) => r.facts.change.id === "asks")?.why).toBe("The author asks you to decide: Ship behind a flag?");
    expect(needs.find((r) => r.facts.change.id === "review-me")?.why).toBe("Waiting for your review");
    expect(groups[2]?.rows[0]?.why).toBe("The agent still owes a screenshot of the UI change");
    expect(groups[3]?.rows[0]?.status.sentence).toBe("Blocked: it needs exclusive editing of src/auth/");
  });

  it("does not ask the author, a past approver, or a signed-out viewer", () => {
    const answered = facts("answered", { cr: { author: LIN, brief: brief({ asks: [{ question: "Q?", options: ["A", "B"] }] }) }, comments: [comment(MARA, 'Decision on "Q?": A', "2026-10-27T10:00:00Z")], landing: landing(true) });
    const approved = facts("approved", { reviews: [review(MARA, "VERDICT_APPROVE", 2)] });
    const mine = facts("mine", { cr: { author: MARA } });
    const g = groupQueue([answered, approved, mine], MARA, NOW);
    expect(g[0]?.rows).toEqual([]);
    expect(g[1]?.rows.map((r) => r.facts.change.id)).toEqual(["answered"]);
    expect(groupQueue([facts("x")], null, NOW)[0]?.rows).toEqual([]);
  });

  it("orders rows newest first and returns every group, even empty ones", () => {
    const a = facts("a", { landing: landing(true), cr: { revisions: [{ n: 1, diffClass: "logic", createdAt: "2026-10-27T01:00:00Z" }] } });
    const b = facts("b", { landing: landing(true), cr: { revisions: [{ n: 1, diffClass: "logic", createdAt: "2026-10-27T02:00:00Z" }] } });
    const g = groupQueue([a, b], MARA, NOW);
    expect(g.map((x) => x.key)).toEqual(["needs-you", "ready", "waiting", "blocked", "landed"]);
    expect(g[1]?.rows.map((r) => r.facts.change.id)).toEqual(["b", "a"]);
  });
});
