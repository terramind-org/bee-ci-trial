/** This week, as one quiet line: what merged, what is open, what agents spent. */
import { usd } from "@/lib/format";
import type { Week } from "@/lib/inbox/progress";
import { noun } from "@/lib/inbox/words";

/** The same three figures as words, for the person's line and the agent's facts. */
export function weekFigures(week: Week): string[] {
  return [
    week.merged + " merged" + (week.merged ? ", " + week.byAgents + " of them by agents on their own" : ""),
    week.open + " " + noun(week.open, "pull request", "pull requests") + " open",
    usd(week.spendMicros) + " spent by agents",
  ];
}

export function ThisWeek({ week }: { week: Week }) {
  return (
    <ul className="flex flex-col gap-1 text-base text-ink-2" data-testid="this-week">
      {weekFigures(week).map((line) => (
        <li key={line}>{line}</li>
      ))}
    </ul>
  );
}
