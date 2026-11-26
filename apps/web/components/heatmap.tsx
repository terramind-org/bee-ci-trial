/**
 * The contribution heatmap (docs/design/tokens.md, "Contribution heatmap"): 53 weeks by 7 days,
 * five steps from `sunken` to full emerald, with steps cut at the quartiles of the year's
 * non-zero counts so a quiet profile still shows shape. Exposed as a table with a hidden caption
 * and a visible text summary; colour is never the only signal.
 */
import { cx } from "./ui";

export interface HeatmapDay {
  /** `YYYY-MM-DD` (UTC). */
  date: string;
  count: number;
}

export interface HeatmapCell {
  date: string;
  count: number;
  step: 0 | 1 | 2 | 3 | 4;
}

export interface HeatmapModel {
  /** 53 columns (weeks, oldest first) of 7 cells (Sunday first); null outside the year. */
  weeks: (HeatmapCell | null)[][];
  total: number;
  /** The Sunday that starts the busiest week, or null when the year is empty. */
  busiestWeek: string | null;
}

const WEEKS = 53;
const DAY_MS = 86_400_000;
const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

const iso = (t: number) => new Date(t).toISOString().slice(0, 10);
const parse = (d: string) => Date.parse(d + "T00:00:00Z");

/** "3 March": day and month, as the summary and cell titles print them. */
export function dayLabel(date: string): string {
  const d = new Date(parse(date));
  return d.getUTCDate() + " " + MONTHS[d.getUTCMonth()];
}

/** Thresholds at the 25th, 50th and 75th percentile of the non-zero counts. */
function quartiles(counts: number[]): [number, number, number] {
  const s = counts.filter((c) => c > 0).sort((a, b) => a - b);
  if (!s.length) return [0, 0, 0];
  const at = (p: number) => s[Math.min(s.length - 1, Math.floor(p * (s.length - 1)))] as number;
  return [at(0.25), at(0.5), at(0.75)];
}

/**
 * Lays the days out ending at `end` (default: the latest day given). Pure: the same days and
 * end give the same grid, so server and client render alike.
 */
export function buildHeatmap(days: HeatmapDay[], end?: string): HeatmapModel {
  const counts = new Map<string, number>();
  for (const d of days) if (Number.isFinite(parse(d.date))) counts.set(d.date, (counts.get(d.date) ?? 0) + Math.max(0, d.count));
  const last = end ?? [...counts.keys()].sort().at(-1);
  const weeks: (HeatmapCell | null)[][] = [];
  if (!last || !Number.isFinite(parse(last))) {
    for (let w = 0; w < WEEKS; w++) weeks.push(Array.from({ length: 7 }, () => null));
    return { weeks, total: 0, busiestWeek: null };
  }
  const endT = parse(last);
  const firstSunday = endT - new Date(endT).getUTCDay() * DAY_MS - (WEEKS - 1) * 7 * DAY_MS;
  const yearStart = endT - 364 * DAY_MS;
  const inYear: number[] = [];
  for (let t = yearStart; t <= endT; t += DAY_MS) inYear.push(counts.get(iso(t)) ?? 0);
  const [q1, q2, q3] = quartiles(inYear);
  const step = (c: number): HeatmapCell["step"] => (c <= 0 ? 0 : c <= q1 ? 1 : c <= q2 ? 2 : c <= q3 ? 3 : 4);
  let total = 0;
  let best = -1;
  let busiestWeek: string | null = null;
  for (let w = 0; w < WEEKS; w++) {
    const col: (HeatmapCell | null)[] = [];
    let weekSum = 0;
    for (let d = 0; d < 7; d++) {
      const t = firstSunday + (w * 7 + d) * DAY_MS;
      if (t < yearStart || t > endT) {
        col.push(null);
        continue;
      }
      const date = iso(t);
      const count = counts.get(date) ?? 0;
      total += count;
      weekSum += count;
      col.push({ date, count, step: step(count) });
    }
    if (weekSum > best && weekSum > 0) {
      best = weekSum;
      busiestWeek = iso(firstSunday + w * 7 * DAY_MS);
    }
    weeks.push(col);
  }
  return { weeks, total, busiestWeek };
}

export function heatmapSummary(m: HeatmapModel): string {
  const n = m.total === 1 ? "1 contribution" : m.total.toLocaleString("en-US") + " contributions";
  return n + " in the last year" + (m.busiestWeek ? ", busiest week of " + dayLabel(m.busiestWeek) : "");
}

const cellText = (c: HeatmapCell) => (c.count === 1 ? "1 contribution" : c.count + " contributions") + " on " + dayLabel(c.date);

const STEP: Record<HeatmapCell["step"], string> = { 0: "", 1: "bg-ok/25", 2: "bg-ok/45", 3: "bg-ok/70", 4: "bg-ok" };

function Swatch({ step }: { step: HeatmapCell["step"] }) {
  return (
    <span aria-hidden="true" className="block size-[11px] rounded-[2px] bg-sunken">
      {step ? <span className={cx("block size-full rounded-[2px]", STEP[step])} /> : null}
    </span>
  );
}

export function Heatmap({ days, end, className }: { days: HeatmapDay[]; end?: string; className?: string }) {
  const m = buildHeatmap(days, end);
  const summary = heatmapSummary(m);
  return (
    <figure className={cx("min-w-0", className)}>
      <figcaption className="mb-2 text-sm text-ink-2">{summary}</figcaption>
      {/* relative: the per-day sr-only labels are absolutely positioned; without a containing
          block here they escape the scroll box and widen the whole page on a phone. */}
      <div className="relative overflow-x-auto scrollbar-thin">
        <table className="border-separate border-spacing-[2px]">
          <caption className="sr-only">Contributions per day over the last year, one column per week</caption>
          <tbody>
            {WEEKDAYS.map((name, d) => (
              <tr key={name}>
                <th scope="row" className="sr-only">
                  {name}
                </th>
                {m.weeks.map((week, w) => {
                  const c = week[d];
                  return (
                    <td key={w} className="p-0" title={c ? cellText(c) : undefined}>
                      {c ? (
                        <>
                          <Swatch step={c.step} />
                          <span className="sr-only">{cellText(c)}</span>
                        </>
                      ) : (
                        <span aria-hidden="true" className="block size-[11px]" />
                      )}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="mt-2 flex items-center justify-end gap-1 text-xs text-ink-3" aria-hidden="true">
        <span className="mr-1">Less</span>
        {([0, 1, 2, 3, 4] as const).map((s) => (
          <Swatch key={s} step={s} />
        ))}
        <span className="ml-1">More</span>
      </div>
    </figure>
  );
}
