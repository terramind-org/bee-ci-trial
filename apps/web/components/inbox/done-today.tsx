/** "Done today": merged work collapsed to one line per repo, opened with a native disclosure. */
import Link from "next/link";
import { Avatar, toAvatar } from "@/components/avatar";
import { Icon } from "@/components/icons";
import { relativeTime } from "@/lib/format";
import { landedTitle, type DoneGroup } from "@/lib/inbox/progress";
import { repoHref } from "@/lib/routes";

export function DoneToday({ groups, now }: { groups: DoneGroup[]; now: number }) {
  return (
    <div className="divide-y divide-line" data-testid="done-today">
      {groups.map((g) => {
        const people = [...new Map(g.items.flatMap((e) => (e.actor ? [[e.actor.id, e.actor] as const] : []))).values()];
        return (
          <details key={g.repo} className="group">
            <summary className="flex min-h-14 cursor-pointer list-none items-center gap-3 rounded-md px-1 text-md transition-colors hover:bg-hover [&::-webkit-details-marker]:hidden">
              <Icon name="chevronRight" size={16} weight="bold" className="text-ink-3 transition-transform duration-200 group-open:rotate-90" />
              <span className="min-w-0 truncate font-medium">{g.repo}</span>
              <span className="shrink-0 text-ink-3">{g.items.length} merged</span>
              <span className="ml-auto flex shrink-0 -space-x-1.5" role="img" aria-label={"by " + people.map((p) => p.display).join(", ")}>
                {people.slice(0, 5).map((p) => (
                  <span key={p.id} className="rounded-full ring-2 ring-bg">
                    <Avatar principal={toAvatar(p)} size={28} />
                  </span>
                ))}
              </span>
            </summary>
            <ul className="pb-3 pl-8">
              {g.items.map((e) => (
                <li key={e.id} className="flex items-center gap-3 py-2 text-base">
                  <span className="min-w-0 flex-1 truncate">
                    <span className="text-ink">{landedTitle(e)}</span>
                    <span className="text-ink-3"> by {e.actor?.display ?? "someone"}</span>
                  </span>
                  <time dateTime={e.createdAt} className="shrink-0 text-ink-3">
                    {relativeTime(e.createdAt, now)}
                  </time>
                </li>
              ))}
              <li className="pt-1">
                <Link href={repoHref(g.repo, "activity")} className="text-base text-ink-2 underline decoration-line-strong underline-offset-4 hover:text-ink hover:decoration-ink">
                  All activity in {g.repo}
                </Link>
              </li>
            </ul>
          </details>
        );
      })}
    </div>
  );
}
