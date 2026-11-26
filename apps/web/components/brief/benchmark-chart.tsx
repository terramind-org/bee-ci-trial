/**
 * Benchmark bars (docs/design/tokens.md "Benchmark bars"): one row per series, the name and unit
 * on the left, then baseline and change as two bars on a shared scale starting at zero, each with
 * its value at the end. The change bar is `ok` when better and `bad` when worse, and the
 * difference is always said in words, so colour never carries the meaning alone. Inline SVG, no
 * chart library; the values are also in a visually hidden table.
 */
import type { BenchmarkSeries } from "@/lib/api/types";
import { benchmarkRows, benchmarkVerdict } from "@/lib/evidence-view";
import { cx } from "../ui";

/** Room left at the end of a bar for its value. */
const VALUE_ROOM = "6.5rem";

function Bar({ fraction, label, value, className, opacity }: { fraction: number; label: string; value: string; className: string; opacity?: number }) {
  const width = "calc((100% - " + VALUE_ROOM + ") * " + Math.max(0, Math.min(1, fraction)).toFixed(4) + ")";
  return (
    <div className="flex h-4 items-center gap-2">
      <svg className={cx("block h-2.5 shrink-0", className)} style={{ width, minWidth: fraction > 0 ? 2 : 0 }} aria-hidden="true" focusable="false" preserveAspectRatio="none" viewBox="0 0 100 10">
        <title>{label + ": " + value}</title>
        <rect width="100" height="10" rx="2" fill="currentColor" opacity={opacity} />
      </svg>
      <span className="num text-sm whitespace-nowrap text-ink-2">{value}</span>
    </div>
  );
}

export function BenchmarkChart({ series, title }: { series: BenchmarkSeries[]; title: string }) {
  const rows = benchmarkRows(series);
  if (!rows.length) return <p className="text-base text-ink-3">No measurements were attached.</p>;
  return (
    <div className="flex flex-col gap-5" data-benchmark>
      <p className="text-base font-medium text-ink">{benchmarkVerdict(series)}</p>
      <ul className="flex flex-col gap-5">
        {rows.map((r) => (
          <li key={r.series.name} className="grid gap-x-4 gap-y-1.5 sm:grid-cols-[minmax(7rem,11rem)_minmax(0,1fr)]" data-series={r.series.name}>
            <div className="min-w-0 text-sm">
              <span className="text-ink [overflow-wrap:anywhere]">{r.series.name}</span>
              {r.series.unit ? <span className="text-ink-3"> ({r.series.unit}, {r.series.lowerIsBetter ? "lower" : "higher"} is better)</span> : <span className="text-ink-3"> ({r.series.lowerIsBetter ? "lower" : "higher"} is better)</span>}
            </div>
            <div className="flex min-w-0 flex-col gap-1">
              <Bar fraction={r.baselineFraction} label="Baseline" value={r.baselineText} className="text-ink-3" opacity={0.4} />
              <Bar fraction={r.valueFraction} label="This change" value={r.valueText} className={r.better === true ? "text-ok" : r.better === false ? "text-bad" : "text-ink-2"} />
              <p className={cx("text-sm", r.better === false ? "text-bad" : r.better === true ? "text-ok" : "text-ink-2")} data-words>
                {r.words}.
              </p>
            </div>
          </li>
        ))}
      </ul>
      <p className="flex items-center gap-4 text-xs text-ink-3" aria-hidden="true">
        <span className="inline-flex items-center gap-1.5">
          <span className="inline-block h-2 w-4 rounded-xs bg-ink-3 opacity-40" /> Baseline
        </span>
        <span className="inline-flex items-center gap-1.5">
          <span className="inline-block h-2 w-4 rounded-xs bg-ink-2" /> This change
        </span>
      </p>
      <table className="sr-only">
        <caption>{title}: baseline and this change</caption>
        <thead>
          <tr>
            <th scope="col">Measure</th>
            <th scope="col">Baseline</th>
            <th scope="col">This change</th>
            <th scope="col">Difference</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.series.name}>
              <th scope="row">{r.series.name}</th>
              <td>{r.baselineText}</td>
              <td>{r.valueText}</td>
              <td>{r.words}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
