// Reads the SPEC §16 table out of the load harness's Markdown report
// (`bee_conformance::load::report`): one row per target, judged hit, miss or not judged.

export interface Target {
  operation: string;
  target: string;
  p50: string;
  p99: string;
  measured: string;
  result: string;
  notes: string;
  status: "hit" | "miss" | "not judged" | "not run";
  /** For a miss: how far over the bound, as the report states it (`29 %`). */
  by?: string;
}

const cells = (line: string): string[] =>
  line
    .trim()
    .replace(/^\||\|$/g, "")
    .split("|")
    .map((c) => c.trim());

/** The rows of the target table (the one whose header starts with `Operation`). */
export function parseTargets(markdown: string): Target[] {
  const lines = markdown.split("\n");
  const start = lines.findIndex((l) => /^\|\s*Operation\s*\|/.test(l));
  if (start < 0) return [];
  const rows: Target[] = [];
  for (const line of lines.slice(start + 2)) {
    if (!line.trim().startsWith("|")) break;
    const c = cells(line);
    if (c.length < 7) continue;
    const [operation, target, p50, p99, measured, result, ...rest] = c as [string, string, string, string, string, string, ...string[]];
    const miss = /\*\*miss\*\*\s*\(by ([^)]+)\)/.exec(result);
    const status: Target["status"] = miss
      ? "miss"
      : /^hit\b/.test(result)
        ? "hit"
        : /^not judged/.test(result)
          ? "not judged"
          : "not run";
    rows.push({ operation, target, p50, p99, measured, result, notes: rest.join("|").trim(), status, by: miss?.[1] });
  }
  return rows;
}

export interface Summary {
  hits: number;
  misses: Target[];
  notJudged: number;
  notRun: Target[];
}

export function summarize(rows: Target[]): Summary {
  return {
    hits: rows.filter((r) => r.status === "hit").length,
    misses: rows.filter((r) => r.status === "miss"),
    notJudged: rows.filter((r) => r.status === "not judged").length,
    notRun: rows.filter((r) => r.status === "not run"),
  };
}
