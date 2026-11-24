import { describe, expect, it } from "vitest";
import { missingInWords, parseApproval, statusOf } from "./status";
import { change, check, landing, LIN, MARA, review } from "./brief-samples";

const pass = (n: number) => Array.from({ length: n }, (_, i) => check("c" + i, "CHECK_STATUS_PASSED"));

describe("status sentence", () => {
  it("reads ready to land with approvals and checks", () => {
    const s = statusOf({ change: change(), landing: landing(true), checks: pass(14), reviews: [review(MARA, "VERDICT_APPROVE", 2), review(LIN, "VERDICT_APPROVE", 2)] });
    expect(s.sentence).toBe("Ready to land: 2 approvals, all 14 checks pass");
    expect(s.kind).toBe("ready");
    expect(s.tone).toBe("ok");
  });

  it("does not count an approval of an older revision", () => {
    const s = statusOf({ change: change(), landing: landing(true), checks: pass(2), reviews: [review(MARA, "VERDICT_APPROVE", 1)] });
    expect(s.sentence).toBe("Ready to land: all 2 checks pass");
  });

  it("puts failing checks before everything but the queue and the end states", () => {
    expect(statusOf({ change: change(), landing: landing(false, ["check:test (failed)"]), checks: [check("test", "CHECK_STATUS_FAILED")], reviews: [] }).sentence).toBe("Checks failing: test check failed");
    expect(statusOf({ change: change({ state: "CHANGE_STATE_LANDED" }), checks: [check("test", "CHECK_STATUS_FAILED")], reviews: [] }).sentence).toBe("Merged");
    expect(statusOf({ change: change({ state: "CHANGE_STATE_CLOSED" }), checks: [], reviews: [] }).headline).toBe("Closed without merging");
    expect(statusOf({ change: change(), checks: [], reviews: [], queue: { position: 2, state: "waiting" } }).sentence).toBe("In the merge queue: position 2");
  });

  it("waits for a required check that has not reported", () => {
    expect(statusOf({ change: change(), landing: landing(false, ["check:e2e (missing)"]), checks: [], reviews: [] }).sentence).toBe("Checks running: waiting for the e2e check");
  });

  it("names the approvals still needed, and changes requested", () => {
    const need = landing(false, ["approval:human:sensitive (0 of 1)"]);
    expect(statusOf({ change: change(), landing: need, checks: pass(3), reviews: [] }).sentence).toBe("Needs review: needs one approval from a person, all 3 checks pass");
    expect(statusOf({ change: change(), landing: need, checks: pass(3), reviews: [review(MARA, "VERDICT_REQUEST_CHANGES", 2)] }).sentence).toBe("Changes requested: needs one approval from a person, all 3 checks pass");
    // Changes asked on r1 and approved on r2: no longer requested.
    expect(statusOf({ change: change(), landing: landing(true), checks: pass(1), reviews: [review(MARA, "VERDICT_REQUEST_CHANGES", 1, "2026-10-27T11:00:00Z"), review(MARA, "VERDICT_APPROVE", 2)] }).kind).toBe("ready");
  });

  it("calls rules no review can clear blocked", () => {
    const s = statusOf({ change: change(), landing: landing(false, ["lease:exclusive:src/auth/"]), checks: pass(1), reviews: [] });
    expect(s.sentence).toBe("Blocked: it needs exclusive editing of src/auth/");
  });

  it("reads landing rules in words", () => {
    expect(parseApproval("approval:owner:src/billing (1 of 2)")).toEqual({ from: "owner", scope: "src/billing", have: 1, need: 2 });
    expect(missingInWords("approval:owner:src/billing (1 of 2)")).toBe("needs one more approval from an owner of src/billing");
    expect(missingInWords("approval:any (0 of 2)")).toBe("needs two approvals");
    expect(missingInWords("approval:any")).toBe("needs an approval");
    expect(missingInWords("approval:owner:src/auth")).toBe("needs an approval from an owner of src/auth");
    expect(missingInWords("stack: parent Add the cache must land first")).toBe("waits for “Add the cache” to merge first");
    expect(missingInWords("check:lint (running)")).toBe("is waiting for the lint check");
    expect(missingInWords("something:new")).toBe("something:new");
  });
});
