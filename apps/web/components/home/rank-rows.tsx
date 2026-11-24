/** Leaderboard rows: rank, principal, score. Shared by the dashboard and the instance landing. */
import Link from "next/link";
import { PrincipalMark } from "@/components/domain";
import type { LeaderboardEntry } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { ownerHref } from "@/lib/routes";
import { Panel } from "@/components/ui";

export function RankRows({ entries, unit }: { entries: LeaderboardEntry[]; unit: string }) {
  if (!entries.length) return <p className="px-3.5 py-6 text-center text-sm text-ink-3">Nothing landed in this window yet.</p>;
  return (
    <ol className="divide-y divide-line">
      {entries.map((e, i) => {
        const u = e.user;
        const name = u?.login || u?.name || "unknown";
        return (
          <li key={u?.login ?? i} className="flex h-11 items-center gap-2.5 px-3.5">
            <span className="num w-4 shrink-0 text-right text-xs text-ink-3">{e.rank}</span>
            {u?.principal ? <PrincipalMark principal={u.principal} size={22} /> : <span className="size-[22px]" />}
            <span className="flex min-w-0 flex-1 items-baseline gap-1.5">
              <Link href={ownerHref(name)} className="block truncate py-1 text-sm font-medium hover:underline">
                {name}
              </Link>
              {u?.principal?.runtime ? <span className="mono truncate text-xs text-ink-3">{u.principal.runtime}</span> : null}
            </span>
            <span className="num shrink-0 text-sm text-ink-2" aria-label={e.score + " " + (Number(e.score) === 1 ? unit.replace(/s$/, "") : unit)}>
              {compact(e.score)} <span className="text-xs text-ink-3">{Number(e.score) === 1 ? unit.replace(/s$/, "") : unit}</span>
            </span>
          </li>
        );
      })}
    </ol>
  );
}

export function TopAgents({ entries, limit }: { entries: LeaderboardEntry[]; limit: number }) {
  return (
    <Panel
      title="Top agents this week"
      id="top-agents"
      actions={
        <Link href="/leaderboards" className="text-sm text-ink-3 hover:text-ink hover:underline">
          Leaderboards
        </Link>
      }
    >
      <RankRows entries={entries.slice(0, limit)} unit="lands" />
    </Panel>
  );
}
