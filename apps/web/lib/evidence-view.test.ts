import { describe, expect, it } from "vitest";
import { benchmarkRows, benchmarkVerdict, benchmarkWords, evidenceIndex, formatValue, galleryOrder, sourceOf, staleness, testReportWords, thumbnailOf, titleOf } from "./evidence-view";
import { evidence, image, LIN } from "./brief-samples";

describe("evidence view", () => {
  it("orders the gallery hero first, then by order, then oldest, and hides pending items", () => {
    const items = [
      evidence("e3", { spec: { order: 1 }, createdAt: "2026-10-27T10:00:00Z" }),
      evidence("e2", { spec: { order: 1 }, createdAt: "2026-10-27T09:00:00Z" }),
      evidence("e1", { spec: { order: 0 } }),
      evidence("e9", { spec: { hero: true, order: 5 } }),
      evidence("e0", { state: "EVIDENCE_STATE_PENDING" }),
    ];
    expect(galleryOrder(items).map((e) => e.id)).toEqual(["e9", "e1", "e2", "e3"]);
  });

  it("labels sources and staleness in words", () => {
    expect(sourceOf(evidence("a")).label).toBe("From the agent");
    expect(sourceOf(evidence("a", { uploader: LIN })).label).toBe("From Lin Chen");
    expect(sourceOf(evidence("a", { source: "EVIDENCE_SOURCE_CHECK_RUN" }))).toEqual({ ci: true, label: "Verified by CI" });
    expect(sourceOf(evidence("a", { source: "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT" })).ci).toBe(true);
    expect(staleness(evidence("a"), 2)).toBeNull();
    expect(staleness(evidence("a", { stale: true, revision: 1 }), 3)).toEqual({ label: "Stale: from r1", title: "Captured before the latest revision (r3); the author may need to capture it again" });
    expect(titleOf(evidence("a", { spec: { kind: "EVIDENCE_KIND_BEFORE_AFTER" } }))).toBe("Before and after");
  });

  it("indexes images for Markdown, using the after image of a pair and a video's poster", () => {
    const pair = evidence("pair", { spec: { kind: "EVIDENCE_KIND_BEFORE_AFTER" }, assets: [image("BEFORE", "/api/evidence/b", "Old"), image("AFTER", "/api/evidence/a", "New")] });
    const video = evidence("vid", { spec: { kind: "EVIDENCE_KIND_VIDEO", title: "Walkthrough" }, assets: [image("POSTER", "/api/evidence/p", "")] });
    const bench = evidence("bench", { spec: { kind: "EVIDENCE_KIND_BENCHMARK" } });
    const idx = evidenceIndex([pair, video, bench]);
    expect(idx.get("pair")).toEqual({ url: "/api/evidence/a", alt: "New" });
    expect(idx.get("vid")).toEqual({ url: "/api/evidence/p", alt: "Walkthrough" });
    expect(idx.has("bench")).toBe(false);
    expect(thumbnailOf(pair)?.url).toBe("/api/evidence/a");
  });

  it("says benchmark differences in words", () => {
    expect(benchmarkWords({ name: "p95", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true })).toBe("35 ms faster (29% better) than the baseline");
    expect(benchmarkWords({ name: "memory", unit: "MB", baseline: 210, value: 224, lowerIsBetter: true })).toBe("14 MB larger (7% worse) than the baseline");
    expect(benchmarkWords({ name: "throughput", unit: "req/s", baseline: 1000, value: 1250, lowerIsBetter: false })).toBe("250 req/s higher (25% better) than the baseline");
    expect(benchmarkWords({ name: "x", unit: "", baseline: 0, value: 2, lowerIsBetter: true })).toBe("2 higher (worse) than the baseline");
    expect(benchmarkWords({ name: "x", unit: "ms", baseline: 5, value: 5, lowerIsBetter: true })).toBe("The same as the baseline");
    expect(formatValue(0.12345)).toBe("0.123");
    expect(formatValue(12.5)).toBe("12.5");
    expect(formatValue(120)).toBe("120");
  });

  it("puts bars on one zero-based scale per unit", () => {
    const rows = benchmarkRows([
      { name: "p95", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true },
      { name: "p50", unit: "ms", baseline: 60, value: 40, lowerIsBetter: true },
    ]);
    expect(rows[0]?.baselineFraction).toBe(1);
    expect(rows[0]?.valueFraction).toBeCloseTo(85 / 120);
    expect(rows[1]?.baselineFraction).toBe(0.5);
    expect(rows[1]?.better).toBe(true);
    const mixed = benchmarkRows([
      { name: "p95", unit: "ms", baseline: 120, value: 85, lowerIsBetter: true },
      { name: "memory", unit: "MB", baseline: 210, value: 224, lowerIsBetter: true },
    ]);
    expect(mixed[1]?.valueFraction).toBe(1);
    expect(mixed[1]?.better).toBe(false);
    expect(benchmarkVerdict(mixed.map((r) => r.series))).toBe("Better on 1 of 2 measures, worse on 1");
  });

  it("says a test report in a sentence", () => {
    expect(testReportWords({ passed: 14, failed: 0, skipped: 0, durationMs: "12000", failures: [], summary: "" })).toBe("14 passed in 12s");
    expect(testReportWords({ passed: 13, failed: 1, skipped: 2, durationMs: "64000", failures: [], summary: "" })).toBe("13 passed, 1 failed and 2 skipped in 1m 4s");
  });
});
