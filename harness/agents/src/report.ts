// The markdown summary: one row per scenario, one column per runtime, cells "passes/runs".
// Suitable for pasting into docs/release-report.md.

import { runtimeSlug, type Results } from "./orchestrate";
import type { Scenario } from "./scenario";

export function summary(results: Results, scenarios: Scenario[]): string {
  const runtimes = results.runtimes.map((r) => r.id);
  const lines: string[] = [];
  lines.push(`## Agent scenarios`);
  lines.push("");
  lines.push(`Server \`${results.server}\`, ${results.runsPerCell} run(s) per cell, ${results.startedAt} to ${results.finishedAt}.`);
  lines.push("");
  for (const r of results.runtimes) {
    lines.push(`- ${runtimeSlug(r.id)}: ${r.available ? `${r.version ?? "?"}` : `unavailable (${r.problem ?? "?"})`}`);
  }
  lines.push("");
  lines.push(`| Scenario | Area | Title | ${runtimes.map(runtimeSlug).join(" | ")} |`);
  lines.push(`|---|---|---|${runtimes.map(() => "---").join("|")}|`);
  for (const s of scenarios) {
    const cells = runtimes.map((rt) => {
      const c = results.cells.find((x) => x.scenario === s.id && x.runtime === rt);
      if (!c) return "-";
      const mark = c.passes === c.runs ? "PASS" : "FAIL";
      const other = c.statuses.filter((x) => x !== "pass" && x !== "fail");
      const note = other.length ? ` (${[...new Set(other)].join(", ")})` : "";
      return `${c.passes}/${c.runs} ${mark}${note}`;
    });
    lines.push(`| ${s.id} | ${s.area} | ${s.title.replace(/\|/g, "\\|")} | ${cells.join(" | ")} |`);
  }
  lines.push("");
  const passing = results.cells.filter((c) => c.passes === c.runs).length;
  lines.push(`${passing}/${results.cells.length} cells pass ${results.runsPerCell}/${results.runsPerCell}.`);
  if (results.runsPerCell < 3) lines.push("Fewer than 3 runs per cell: this is not a PRODUCT §3.1 pass-bar run.");
  lines.push(results.ok ? "Result: PASS" : "Result: FAIL");
  return lines.join("\n") + "\n";
}
