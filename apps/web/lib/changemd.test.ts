import { describe, expect, it } from "vitest";
import { pathBytes } from "./api/ids";
import { changeActions, renderChangeMd, unifiedDiff, type ChangeMdInput } from "./changemd";
import { AGENT, brief, change, check, checklist, comment, diffItem, evidence, image, landing, MARA, review } from "./brief-samples";

const input: ChangeMdInput = {
  repo: "acme/api",
  origin: "https://bee.example",
  change: change({
    body: "Rotates tokens.\n\n## Notes\nNone.",
    brief: brief({
      outcome: "Sessions rotate when a role changes",
      highlights: [{ text: "Tokens rotate on role change", paths: ["src/auth/session.ts"] }],
      asks: [{ question: "Sign out other devices?", options: ["Only this session", "Every device"] }],
      risks: [{ text: "Users could be signed out mid-request", mitigation: "The old token stays valid for 30 s" }],
      rollback: "Revert the change; tokens stay valid.",
      tryItUrl: "https://preview.example/rotate",
      writtenForRevision: 1,
      author: AGENT,
    }),
  }),
  checklist: checklist(false, [{ id: "brief:risk", detail: "a risk or a rollback for the sensitive paths", kinds: [], paths: ["src/auth/session.ts"], satisfied: true, evidenceIds: [] }, { id: "evidence:ui", detail: "a screenshot of the UI change", kinds: [], paths: ["web/a.tsx"], satisfied: false, evidenceIds: [] }]),
  evidence: [
    evidence("ev-shot", { spec: { title: "Settings page", hero: true, caption: "The new toggle" }, assets: [image("MAIN", "/api/evidence/ev-shot/main?sig=1", "Settings with a toggle")] }),
    evidence("ev-bench", { stale: true, revision: 1, source: "EVIDENCE_SOURCE_CHECK_RUN", checkRunId: "ck-bench", spec: { kind: "EVIDENCE_KIND_BENCHMARK", title: "Latency", benchmark: [{ name: "p95", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true }] } }),
  ],
  checks: [check("test", "CHECK_STATUS_PASSED", { summary: "14 passed" }), check("lint", "CHECK_STATUS_FAILED")],
  reviews: [review(MARA, "VERDICT_REQUEST_CHANGES", 1, "2026-10-27T11:00:00Z", "Add a test.")],
  threads: [{ id: "t1", changeRequestId: "cr-1", path: pathBytes("src/auth/session.ts"), side: "new", startLine: 3, endLine: 3, revision: 2, resolved: false, outdated: false, comments: [{ id: "tc", author: MARA, body: "Why 30 s?", suggestion: "const GRACE = 10;", createdAt: "2026-10-27T12:10:00Z" }] }],
  comments: [comment(MARA, 'Decision on "Sign out other devices?": Every device', "2026-10-27T12:20:00Z")],
  diff: [diffItem("src/auth/session.ts", "sensitive", [" ctx", "-old", "+new ```"]), diffItem("README.md", "docs", ["+hi"])],
  landing: landing(false, ["check:lint (failed)"], ["approval:human:sensitive (1 of 1)"]),
};

describe("change.md", () => {
  it("renders every part in order", () => {
    const md = renderChangeMd(input);
    const heads = md.split("\n").filter((l) => l.startsWith("## "));
    expect(heads).toEqual(["## Brief", "## Checklist", "## Evidence", "## Checks", "## Reviews", "## Threads", "## Conversation", "## What changed", "## Files", "## Diff", "## Act on it"]);
    expect(md).toContain("# Rotate session tokens on role change (#12)");
    expect(md).toContain("- Status: Checks failing: lint check failed");
    expect(md).toContain("**Outcome:** Sessions rotate when a role changes");
    expect(md).toContain("Written for r1 by triage (agent, claude-code) (older than r2).");
    expect(md).toContain("Decided: “Every device” by Mara Okafor at 2026-10-27T12:20:00Z.");
    expect(md).toContain("- [ ] `evidence:ui`: a screenshot of the UI change Paths: `web/a.tsx`.");
    expect(md).toContain("- main: https://bee.example/api/evidence/ev-shot/main?sig=1 (image/png, 640×400, sha256:");
    expect(md).toContain("- Source: Verified by CI (check run ck-bench), added by triage (agent, claude-code)");
    expect(md).toContain("Stale: from r1");
    expect(md).toContain("- p95: 120 → 85 ms; 35 ms faster (29% better) than the baseline.");
    expect(md).toContain("- Missing: `check:lint (failed)` (it failed the lint check)");
    expect(md).toContain("- Mara Okafor asked for changes on r1 (an earlier revision) at 2026-10-27T11:00:00Z:\n  > Add a test.");
    expect(md).toContain("```suggestion");
    expect(md).toContain("> ## Notes");
    expect(md).toContain("- Sensitive changes in src/auth (1 file), without tests. “Tokens rotate on role change”");
    expect(md).toContain("- `src/auth/session.ts` modified, sensitive, +1 −1");
    // The diff contains three backticks, so its fence is longer.
    expect(md).toContain("````diff\ndiff --bee a/README.md b/README.md");
    expect(md).toContain("bee cr land");
    expect(md).toContain("HTTP: `POST /bee.v1.ChangeService/EnqueueChange`");
    expect(md).not.toMatch(/\n\n\n/);
    expect(md.endsWith("\n")).toBe(true);
  });

  it("is the same bytes for the same inputs in any order", () => {
    const shuffled: ChangeMdInput = { ...input, evidence: [...(input.evidence ?? [])].reverse(), checks: [...(input.checks ?? [])].reverse(), diff: [...(input.diff ?? [])].reverse() };
    expect(renderChangeMd(shuffled)).toBe(renderChangeMd(input));
  });

  it("writes git-style unified diffs, with adds and deletes against /dev/null", () => {
    expect(unifiedDiff([diffItem("new.ts", "logic", ["+a"], "added")])).toBe("diff --bee a/new.ts b/new.ts\n--- /dev/null\n+++ b/new.ts\n@@ -1,0 +1,1 @@\n+a");
    expect(unifiedDiff([diffItem("gone.ts", "logic", ["-a"], "deleted")])).toContain("+++ /dev/null");
  });

  it("offers answers to asks and leaves review and land out once merged", () => {
    const open = changeActions({ repo: "acme/api", change: input.change, comments: [] }).map((a) => a.title);
    expect(open).toContain("Answer “Sign out other devices?”");
    expect(open).toContain("Land through the merge queue");
    const merged = changeActions({ repo: "acme/api", change: { ...input.change, state: "CHANGE_STATE_LANDED" }, comments: input.comments }).map((a) => a.title);
    expect(merged).toContain("Change the answer to “Sign out other devices?”");
    expect(merged).not.toContain("Land through the merge queue");
  });
});
