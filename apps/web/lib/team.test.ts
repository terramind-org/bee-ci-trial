import { describe, expect, it } from "vitest";
import { runtimeName, teamOf } from "./team";
import { AGENT, brief, change, comment, LIN, MARA, review } from "./brief-samples";

describe("team", () => {
  it("lists agents first with roles, models, cost and elapsed time", () => {
    const t = teamOf({
      change: change({ brief: brief({ outcome: "x", author: AGENT, updatedAt: "2026-10-27T12:05:00Z" }) }),
      reviews: [review(MARA, "VERDICT_REQUEST_CHANGES", 1, "2026-10-27T11:00:00Z"), review(MARA, "VERDICT_APPROVE", 2, "2026-10-27T13:00:00Z"), review(LIN, "VERDICT_COMMENT", 2, "2026-10-27T13:30:00Z")],
      comments: [comment(MARA, "Looks right.", "2026-10-27T12:45:00Z"), comment(AGENT, "Thanks.", "2026-10-27T12:50:00Z")],
      sessions: [
        { session: "s2", runId: "", agentPrincipalId: AGENT.id, model: "claude-opus-5-5", runtime: "claude-code", promptDigest: "", chain: [], changesets: [], changeRequestIds: [], cost: { inputTokens: "1000", outputTokens: "200", usdMicros: "800000" }, createdAt: "2026-10-27T11:30:00Z" },
        { session: "s1", runId: "", agentPrincipalId: AGENT.id, model: "claude-sonnet-5-5", runtime: "claude-code", promptDigest: "", chain: [], changesets: [], changeRequestIds: [], cost: { inputTokens: "500", outputTokens: "100", usdMicros: "400000" }, createdAt: "2026-10-27T09:00:00Z" },
      ],
    });
    expect(t.members.map((m) => m.principal?.display)).toEqual(["triage", "Mara Okafor", "Lin Chen"]);
    const [agent, mara, lin] = t.members;
    expect(agent?.roles).toEqual(["author", "brief", "ran a session"]);
    expect(agent?.rolesText).toBe("Wrote the change and the brief and ran a session");
    expect(agent?.models).toEqual(["claude-opus-5-5", "claude-sonnet-5-5"]);
    expect(agent?.sessions).toBe(2);
    expect(agent?.costMicros).toBe(1_200_000);
    expect(agent?.elapsedMs).toBe((12 * 60 + 50 - 9 * 60) * 60_000);
    expect(mara?.roles).toEqual(["approved", "requested changes"]);
    expect(mara?.rolesText).toBe("Approved and asked for changes");
    expect(lin?.rolesText).toBe("Reviewed");
    expect(t.costMicros).toBe(1_200_000);
    expect(t.models).toEqual(["claude-opus-5-5", "claude-sonnet-5-5"]);
    expect(t.elapsedMs).toBe(4.5 * 3_600_000);
  });

  it("keeps a session whose agent the page did not load, by id", () => {
    const t = teamOf({ change: change({ author: MARA }), sessions: [{ session: "s", runId: "", agentPrincipalId: "p-other", model: "m", runtime: "", promptDigest: "", chain: [], changesets: [], changeRequestIds: [], createdAt: "2026-10-27T10:00:00Z" }] });
    expect(t.members[0]).toMatchObject({ id: "p-other", agent: true, principal: undefined, roles: ["ran a session"] });
    expect(t.members[1]?.rolesText).toBe("Wrote the change");
  });

  it("names runtimes in words", () => {
    expect(runtimeName("claude-code/3.2")).toBe("Claude Code");
    expect(runtimeName("open-hands")).toBe("Open Hands");
    expect(runtimeName("")).toBe("");
  });
});
