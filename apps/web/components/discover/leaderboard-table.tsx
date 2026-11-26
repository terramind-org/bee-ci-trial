import { EmptyState } from "@/components/ui";
import type { LeaderboardEntry } from "@/lib/api/types";
import { PrincipalCell, RepoCell, ShareBar, isTop } from "./rank-row";

/**
 * One leaderboard as a ranked list: rank, who or what, the score, and its share of everything
 * listed as a hairline bar. The bar is decoration: the share is also printed as a number, so
 * nothing depends on seeing it.
 */
export function LeaderboardTable({ entries, unit, caption }: { entries: LeaderboardEntry[]; unit: string; caption: string }) {
  if (entries.length === 0) return <EmptyState title="Nobody on this board yet" icon="dot" />;
  const total = entries.reduce((n, e) => n + Number(e.score), 0);
  return (
    <div>
      <div className="flex items-baseline justify-between gap-4 pb-2 text-sm text-ink-3">
        <span>{entries[0]?.repo ? "Repository" : "Name"}</span>
        <span>{unit}</span>
      </div>
      <ol aria-label={caption} className="rule">
        {entries.map((e, i) => {
          const rank = e.rank || i + 1;
          const score = Number(e.score);
          const share = total > 0 ? score / total : 0;
          const subject = e.user ? <PrincipalCell hit={e.user} top={isTop(rank)} /> : e.repo ? <RepoCell hit={e.repo} top={isTop(rank)} /> : null;
          if (!subject) return null;
          return (
            <li key={e.user?.login ?? e.repo?.fullName ?? i} className="border-b border-line py-4" data-rank={rank}>
              <div className="flex items-center gap-2 sm:gap-4">
                <span className={"num w-8 shrink-0 text-right text-lg " + (isTop(rank) ? "font-semibold text-ink" : "text-ink-3")}>{rank}</span>
                <div className="min-w-0 flex-1">{subject}</div>
                <span className={"num shrink-0 text-xl " + (isTop(rank) ? "font-semibold text-ink" : "text-ink")}>{score}</span>
                <span className="num w-10 shrink-0 text-right text-sm text-ink-3">{Math.round(share * 100)}%</span>
              </div>
              <div className="mt-3 pl-12">
                <ShareBar share={share} />
              </div>
            </li>
          );
        })}
      </ol>
    </div>
  );
}
