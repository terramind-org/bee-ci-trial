/** Merged today: one line per repository, opened with a native disclosure. */
import Link from "next/link";
import { Icon } from "@/components/icons";
import { relativeTime } from "@/lib/format";
import { landedTitle, type DoneGroup } from "@/lib/inbox/progress";
import { repoHref } from "@/lib/routes";

export function DoneToday({ groups, now }: { groups: DoneGroup[]; now: number }) {
  return (
    <div className="divide-y divide-line" data-testid="done-today">
      {groups.map((g) => (
        <details key={g.repo} className="group">
          <summary className="flex min-h-11 cursor-pointer list-none items-center gap-3 text-md transition-colors hover:bg-hover [&::-webkit-details-marker]:hidden">
            <Icon name="chevronRight" size={14} weight="bold" className="text-ink-3 transition-transform duration-200 group-open:rotate-90" />
            <span className="min-w-0 truncate">{g.repo}</span>
            <span className="shrink-0 text-ink-2">{g.items.length} merged</span>
          </summary>
          <ul className="pb-3 pl-7">
            {g.items.map((e) => (
              <li key={e.id} className="flex items-baseline gap-3 py-1.5 text-base">
                <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">
                  <span className="text-ink">{landedTitle(e)}</span>
                  <span className="text-ink-2"> by {e.actor?.display ?? "someone"}</span>
                </span>
                <time dateTime={e.createdAt} className="shrink-0 text-xs text-ink-2">
                  {relativeTime(e.createdAt, now)}
                </time>
              </li>
            ))}
            <li className="pt-1">
              <Link href={repoHref(g.repo, "activity")} className="link text-base">
                All activity in {g.repo}
              </Link>
            </li>
          </ul>
        </details>
      ))}
    </div>
  );
}
