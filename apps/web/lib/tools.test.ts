import { describe, expect, it } from "vitest";
import { NEEDS_PERSON, PAGE_IDS, allOps, isOp, kindOf, pageTools, toolName } from "./tools";

describe("page tools", () => {
  it("lists only operations the method table has, each named after the call behind it", () => {
    const known = new Set(allOps());
    expect(known.size).toBeGreaterThan(100);
    for (const page of PAGE_IDS) {
      const tools = pageTools(page);
      expect(tools.length).toBeGreaterThan(0);
      for (const t of tools) {
        expect(known.has(t.op), page + " lists " + t.op).toBe(true);
        expect(t.name).toMatch(/^[a-z]+(_[a-z0-9]+)+$/);
        expect(t.what.trim()).not.toBe("");
      }
      // A screen never offers the same tool twice.
      expect(new Set(tools.map((t) => t.name)).size).toBe(tools.length);
    }
  });

  it("names a tool after the bee.v1 call, as Copy as does", () => {
    expect(toolName("ChangeService/GetChange")).toBe("change_get_change");
    // The view's name and the wire's differ: the tool is the wire's.
    expect(toolName("ChangeService/CommentChange")).toBe("change_create_change_comment");
    expect(toolName("ChangeService/ListQueue")).toBe("change_get_merge_queue");
    // One view operation, two calls: the request says which.
    expect(toolName("IssueService/SetIssueState", { state: "closed" })).toBe("issue_close_issue");
    expect(toolName("IssueService/SetIssueState", { state: "open" })).toBe("issue_reopen_issue");
  });

  it("marks reads, writes and what needs a person", () => {
    expect(kindOf("ChangeService/GetChange")).toBe("read");
    expect(kindOf("ChangeService/ListChanges")).toBe("read");
    expect(kindOf("ChangeService/EvaluateLanding")).toBe("read");
    expect(kindOf("QueryService/Log")).toBe("read");
    expect(kindOf("AuthService/WhoAmI")).toBe("read");
    expect(kindOf("ChangeService/CommentChange")).toBe("write");
    expect(kindOf("IssueService/CreateIssue")).toBe("write");
    expect(kindOf("IssueService/SetIssueState")).toBe("needs a person");
    expect(kindOf("ChangeService/SubmitReview")).toBe("needs a person");
    expect(kindOf("ChangeService/Enqueue")).toBe("needs a person");
    expect(kindOf("ChangeService/CloseChange")).toBe("needs a person");
    expect(kindOf("SwarmService/SetSwarmState", { state: "SWARM_STATE_PAUSED" })).toBe("write");
    expect(kindOf("SwarmService/SetSwarmState", { state: "SWARM_STATE_STOPPED" })).toBe("needs a person");
    expect(kindOf("SwarmService/DecideSwarmJoin")).toBe("needs a person");
    expect(kindOf("AgentService/CancelAgentRun")).toBe("needs a person");
  });

  it("keeps every operation that needs a person in the method table", () => {
    for (const op of NEEDS_PERSON) expect(isOp(op), op).toBe(true);
    expect(isOp("ChangeService/MergeEverything")).toBe(false);
  });

  it("never calls a method that changes something a read", () => {
    const writes = /^(Create|Set|Update|Delete|Add|Remove|Approve|Withdraw|Close|Submit|Enqueue|Land|Mint|Revoke|Start|Cancel|Assign|Comment|Move|Star|Unstar|Follow|Unfollow|Like|Unlike|Repost|Undo|Dismiss|Reply|Apply|Open|Acquire|Release|Begin|Finalize|Clear|Change|Rerun|Login)/;
    for (const op of allOps()) {
      const method = op.slice(op.indexOf("/") + 1);
      if (writes.test(method)) expect(kindOf(op), op).not.toBe("read");
    }
  });

  it("puts reads first, then writes, then what needs a person", () => {
    const kinds = pageTools("pull").map((t) => t.kind);
    const rank = { read: 0, write: 1, "needs a person": 2 } as const;
    expect([...kinds].sort((a, b) => rank[a] - rank[b])).toEqual(kinds);
    expect(kinds).toContain("needs a person");
    expect(pageTools("code").every((t) => t.kind === "read")).toBe(true);
  });
});
