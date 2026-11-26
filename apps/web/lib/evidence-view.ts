/**
 * Evidence as the Brief tab shows it (docs/spec/web-ui.md §3, docs/design/tokens.md "Brief
 * patterns"): gallery order, source chips, stale labels, benchmark bars and differences in words,
 * test reports in a sentence, and the id → image index that Markdown `evidence:` images resolve
 * against. Pure functions of the Evidence the data layer returns (`lib/api/ops/briefs.ts`, whose
 * asset URLs are already same-origin).
 */
import type { BenchmarkSeries, Evidence, EvidenceAsset, EvidenceAssetRole, EvidenceKind, TestReport } from "./api/types";
import { duration } from "./format";

export const KIND_LABEL: Record<EvidenceKind, string> = {
  EVIDENCE_KIND_UNSPECIFIED: "Evidence",
  EVIDENCE_KIND_SCREENSHOT: "Screenshot",
  EVIDENCE_KIND_BEFORE_AFTER: "Before and after",
  EVIDENCE_KIND_VIDEO: "Video",
  EVIDENCE_KIND_BENCHMARK: "Benchmark",
  EVIDENCE_KIND_IMAGE: "Images",
  EVIDENCE_KIND_AUDIO: "Audio",
  EVIDENCE_KIND_DOCUMENT: "Document",
  EVIDENCE_KIND_PREVIEW_LINK: "Preview",
  EVIDENCE_KIND_TEST_REPORT: "Test report",
};

export function kindOf(e: Evidence): EvidenceKind {
  return e.spec?.kind ?? "EVIDENCE_KIND_UNSPECIFIED";
}

/** The title to show: the spec's, or the kind's label. */
export function titleOf(e: Evidence): string {
  return e.spec?.title.trim() || KIND_LABEL[kindOf(e)];
}

export function isCi(e: Pick<Evidence, "source">): boolean {
  return e.source === "EVIDENCE_SOURCE_CHECK_RUN" || e.source === "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT";
}

export interface Source {
  ci: boolean;
  /** "Verified by CI", "From the agent" or "From Mara Okafor". */
  label: string;
}

export function sourceOf(e: Pick<Evidence, "source" | "uploader">): Source {
  if (isCi(e)) return { ci: true, label: "Verified by CI" };
  const u = e.uploader;
  if (!u || u.kind === "PRINCIPAL_KIND_AGENT" || u.kind === "PRINCIPAL_KIND_SERVICE") return { ci: false, label: "From the agent" };
  return { ci: false, label: "From " + (u.display || "the author") };
}

export interface Staleness {
  /** "Stale: from r2". */
  label: string;
  /** "Captured before the latest revision (r3); the author may need to capture it again". */
  title: string;
}

/**
 * The stale badge, or null when the item is current. `stale` comes from the server; `latest`
 * (the change's newest revision) only fills in the words.
 */
export function staleness(e: Pick<Evidence, "stale" | "revision">, latest: number): Staleness | null {
  if (!e.stale) return null;
  const newest = Math.max(latest, e.revision + 1);
  return { label: "Stale: from r" + e.revision, title: "Captured before the latest revision (r" + newest + "); the author may need to capture it again" };
}

const time = (iso: string | undefined) => (iso ? Date.parse(iso) || 0 : 0);

/** Ready items only, hero first, then by the spec's order, then oldest first, then by id. */
export function galleryOrder(items: Evidence[]): Evidence[] {
  return items
    .filter((e) => e.state !== "EVIDENCE_STATE_PENDING")
    .sort(
      (a, b) =>
        Number(!!b.spec?.hero) - Number(!!a.spec?.hero) ||
        (a.spec?.order ?? 0) - (b.spec?.order ?? 0) ||
        time(a.createdAt) - time(b.createdAt) ||
        (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
    );
}

const ROLE: Record<string, EvidenceAssetRole> = {
  main: "EVIDENCE_ASSET_ROLE_MAIN",
  before: "EVIDENCE_ASSET_ROLE_BEFORE",
  after: "EVIDENCE_ASSET_ROLE_AFTER",
  poster: "EVIDENCE_ASSET_ROLE_POSTER",
  captions: "EVIDENCE_ASSET_ROLE_CAPTIONS",
};

/** The first asset with a role ("main", "before", …). */
export function assetOf(e: Evidence, role: keyof typeof ROLE): EvidenceAsset | undefined {
  return e.assets.find((a) => a.role === ROLE[role] && a.url);
}

/** Every main asset, for image sets. */
export function mainAssets(e: Evidence): EvidenceAsset[] {
  return e.assets.filter((a) => a.role === "EVIDENCE_ASSET_ROLE_MAIN" && a.url);
}

export function isImage(a: Pick<EvidenceAsset, "contentType"> | undefined): boolean {
  return !!a && a.contentType.startsWith("image/");
}

/** The picture that stands for an item in a thumbnail or a Markdown image, when it has one. */
export function thumbnailOf(e: Evidence): EvidenceAsset | undefined {
  switch (kindOf(e)) {
    case "EVIDENCE_KIND_BEFORE_AFTER":
      return assetOf(e, "after") ?? assetOf(e, "before");
    case "EVIDENCE_KIND_VIDEO":
      return assetOf(e, "poster");
    default: {
      const main = mainAssets(e).find(isImage);
      return main ?? (isImage(assetOf(e, "poster")) ? assetOf(e, "poster") : undefined);
    }
  }
}

/** Markdown `![alt](evidence:<id>)` resolves against this: id → the image and its alt text. */
export type EvidenceIndex = Map<string, { url: string; alt: string }>;

export function evidenceIndex(items: Evidence[]): EvidenceIndex {
  const out: EvidenceIndex = new Map();
  for (const e of items) {
    if (e.state === "EVIDENCE_STATE_PENDING") continue;
    const a = thumbnailOf(e);
    if (a?.url) out.set(e.id, { url: a.url, alt: a.alt || titleOf(e) });
  }
  return out;
}

// ---------------------------------------------------------------- benchmarks

const TIME_UNITS = new Set(["ns", "us", "µs", "μs", "ms", "s", "sec", "secs", "seconds", "min", "mins", "minutes", "h"]);
const SIZE_UNITS = new Set(["b", "bytes", "kb", "kib", "mb", "mib", "gb", "gib", "tb"]);

/** Up to three significant digits, never exponent notation, trailing zeros dropped. */
export function formatValue(n: number): string {
  if (!Number.isFinite(n)) return "";
  const a = Math.abs(n);
  const digits = a >= 100 ? 0 : a >= 10 ? 1 : a >= 1 ? 2 : 3;
  return n.toFixed(digits).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

function withUnit(n: number, unit: string): string {
  const u = unit.trim();
  if (!u) return formatValue(n);
  if (u === "%") return formatValue(n) + "%";
  return formatValue(n) + " " + u;
}

export function isBetter(s: BenchmarkSeries): boolean | null {
  if (s.value === s.baseline) return null;
  return s.lowerIsBetter ? s.value < s.baseline : s.value > s.baseline;
}

/**
 * The difference in words, after tokens.md: "35 ms faster (29% better) than the baseline",
 * "14 MB larger (7% worse) than the baseline", "the same as the baseline".
 */
export function benchmarkWords(s: BenchmarkSeries): string {
  const better = isBetter(s);
  if (better === null) return "The same as the baseline";
  const diff = Math.abs(s.value - s.baseline);
  const unit = s.unit.trim().toLowerCase();
  const up = s.value > s.baseline;
  const dir = TIME_UNITS.has(unit) ? (up ? "slower" : "faster") : SIZE_UNITS.has(unit) ? (up ? "larger" : "smaller") : up ? "higher" : "lower";
  const pct = s.baseline !== 0 ? Math.round((diff / Math.abs(s.baseline)) * 100) : null;
  const judged = better ? "better" : "worse";
  return withUnit(diff, s.unit) + " " + dir + " (" + (pct === null ? judged : (pct === 0 ? "<1" : pct) + "% " + judged) + ") than the baseline";
}

export interface BenchmarkRow {
  series: BenchmarkSeries;
  better: boolean | null;
  /** Bar lengths as a fraction (0–1) of the shared scale, which starts at zero. */
  baselineFraction: number;
  valueFraction: number;
  baselineText: string;
  valueText: string;
  words: string;
}

/**
 * Bars on one scale per chart, starting at zero, so lengths compare honestly. Series in different
 * units still share the chart, so each row is scaled to its own larger value instead.
 */
export function benchmarkRows(series: BenchmarkSeries[]): BenchmarkRow[] {
  const units = new Set(series.map((s) => s.unit.trim()));
  const shared = units.size === 1 ? Math.max(0, ...series.flatMap((s) => [Math.abs(s.baseline), Math.abs(s.value)])) : 0;
  return series.map((s) => {
    const scale = shared || Math.max(Math.abs(s.baseline), Math.abs(s.value));
    const frac = (v: number) => (scale > 0 ? Math.abs(v) / scale : 0);
    return {
      series: s,
      better: isBetter(s),
      baselineFraction: frac(s.baseline),
      valueFraction: frac(s.value),
      baselineText: withUnit(s.baseline, s.unit),
      valueText: withUnit(s.value, s.unit),
      words: benchmarkWords(s),
    };
  });
}

/** A one-line verdict over all series: "Better on 1 of 2 measures" / "Better on every measure". */
export function benchmarkVerdict(series: BenchmarkSeries[]): string {
  const judged = series.map(isBetter);
  const better = judged.filter((b) => b === true).length;
  const worse = judged.filter((b) => b === false).length;
  if (!series.length) return "No measures";
  if (better === series.length) return series.length === 1 ? "Better than the baseline" : "Better on every measure";
  if (worse === series.length) return series.length === 1 ? "Worse than the baseline" : "Worse on every measure";
  if (!better && !worse) return "The same as the baseline";
  return "Better on " + better + " of " + series.length + " measures" + (worse ? ", worse on " + worse : "");
}

// ---------------------------------------------------------------- test reports

/** "14 passed in 12s", "13 passed, 1 failed and 2 skipped in 1m 4s". */
export function testReportWords(r: TestReport): string {
  const parts = [r.passed + " passed"];
  if (r.failed) parts.push(r.failed + " failed");
  if (r.skipped) parts.push(r.skipped + " skipped");
  const joined = parts.length === 1 ? parts[0] : parts.slice(0, -1).join(", ") + " and " + parts[parts.length - 1];
  const ms = Number(r.durationMs);
  return joined + (ms > 0 ? " in " + duration(ms) : "");
}

export function testReportPassed(r: TestReport): boolean {
  return r.failed === 0;
}
