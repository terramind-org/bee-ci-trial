import { describe, expect, it } from "vitest";
import type { Run, RunJob } from "./api/types";
import { runSummary } from "./runsummary";

function job(name: string, status: RunJob["status"], failedStep?: string): RunJob {
  return { id: name, name, status, runner: "", steps: failedStep ? [{ name: "checkout", status: "passed", durationMs: "1000" }, { name: failedStep, status: "failed", durationMs: "2000" }] : [] };
}
function run(status: Run["status"], jobs: RunJob[], over: Partial<Run> = {}): Run {
  return { id: "run-1", repoId: "r", workflow: "ci", number: 7, trigger: "change_request", ref: "main", changeRequestId: "", status, jobs, annotations: [], attempt: 1, startedAt: "2026-10-27T10:00:00Z", finishedAt: "2026-10-27T10:03:12Z", ...over };
}

describe("run summary", () => {
  it("says what failed, where, and how long it took", () => {
    const s = runSummary(run("failed", [job("build", "passed"), job("lint", "passed"), job("test", "failed", "cargo test"), job("docs", "passed")], { annotations: [{ path: "a.rs", line: 1, level: "failure", title: "", message: "" }, { path: "b.rs", line: 2, level: "warning", title: "", message: "" }, { path: "c.rs", line: 3, level: "notice", title: "", message: "" }] }));
    expect(s.sentence).toBe("Failed after 3m 12s: 1 of 4 jobs failed (test)");
    expect(s.detail).toBe("The step “cargo test” failed in test. 3 annotations point at code.");
    expect(s.tone).toBe("bad");
  });

  it("covers passing, cancelled, running and queued runs", () => {
    expect(runSummary(run("passed", [job("a", "passed"), job("b", "passed")])).sentence).toBe("Passed in 3m 12s: all 2 jobs passed");
    expect(runSummary(run("passed", [job("a", "passed")])).sentence).toBe("Passed in 3m 12s: the one job passed");
    expect(runSummary(run("cancelled", [job("a", "passed"), job("b", "cancelled")])).sentence).toBe("Cancelled after 3m 12s: 1 of 2 jobs finished before it stopped");
    const running = run("running", [job("a", "passed"), job("e2e", "running"), job("c", "queued")], { finishedAt: undefined });
    expect(runSummary(running, Date.parse("2026-10-27T10:01:03Z")).sentence).toBe("Running for 1m 3s: 1 of 3 jobs done, e2e is running");
    expect(runSummary(running).sentence).toBe("Running: 1 of 3 jobs done, e2e is running");
    expect(runSummary(run("queued", [job("a", "queued")], { startedAt: undefined, finishedAt: undefined })).sentence).toBe("Queued: 1 job waiting for a runner");
  });
});
