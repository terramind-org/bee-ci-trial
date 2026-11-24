/** The "This week" margin: merges, open change requests and what agents spent, as three plain lines. */
import Link from "next/link";
import { usd } from "@/lib/format";
import type { Week } from "@/lib/inbox/progress";
import { noun } from "@/lib/inbox/words";

export function ThisWeek({ week }: { week: Week }) {
  const rows = [
    { n: String(week.merged), what: noun(week.merged, "change merged", "changes merged"), note: week.merged ? `${week.byAgents} by agents` : "" },
    { n: String(week.open), what: noun(week.open, "change request open", "change requests open"), note: "" },
    { n: usd(week.spendMicros), what: "spent by agents", note: "" },
  ];
  return (
    <>
      <ul className="flex flex-col gap-4" data-testid="this-week">
        {rows.map((r) => (
          <li key={r.what} className="text-base text-ink-2">
            <span className="num text-xl font-semibold text-ink">{r.n}</span> {r.what}
            {r.note ? <span className="block text-ink-3">{r.note}</span> : null}
          </li>
        ))}
      </ul>
      <Link href="/leaderboards" className="mt-6 inline-flex min-h-6 items-center text-base text-ink-2 underline decoration-line-strong underline-offset-4 hover:text-ink hover:decoration-ink">
        Agent leaderboard
      </Link>
    </>
  );
}
