"use client";

/**
 * The side rail's revisions: each one opens the Code tab at that revision, and "Changes since"
 * opens it showing only what changed after an earlier one. On the Code tab the viewed revision is
 * the current page; elsewhere the latest is marked but nothing claims `aria-current`.
 */
import Link from "next/link";
import { useSearchParams, useSelectedLayoutSegment } from "next/navigation";
import { DiffClassBadge } from "@/components/domain";
import { Time } from "@/components/time";
import { cx } from "@/components/ui";
import { codeHref, sinceRevision, tabOf, viewedRevision } from "@/lib/change-page";

export interface RevisionRow {
  n: number;
  head: string;
  diffClass: string;
  createdAt?: string;
}

export function RevisionList({ repo, id, revisions }: { repo: string; id: string; revisions: RevisionRow[] }) {
  const onCode = tabOf(useSelectedLayoutSegment()) === "code";
  const sp = useSearchParams();
  const latest = revisions.length;
  const viewing = onCode ? viewedRevision(sp.get("rev") ?? "", latest) : latest;
  const since = onCode ? sinceRevision(sp.get("since") ?? "", viewing) : 0;
  return (
    <>
      <ol className="flex flex-col">
        {[...revisions].reverse().map((r) => {
          const current = r.n === viewing && !since;
          return (
            <li key={r.n}>
              <Link
                href={codeHref(repo, id, r.n === latest ? undefined : r.n)}
                aria-current={onCode && current ? "page" : undefined}
                className={cx("-mx-2 flex min-h-9 items-center gap-2 rounded-md px-2 py-1 text-sm hover:bg-hover", current && "bg-hover")}
              >
                <span className="num w-7 font-semibold text-ink">r{r.n}</span>
                <span className="mono min-w-0 flex-1 truncate text-xs text-ink-3">{r.head}</span>
                <DiffClassBadge cls={r.diffClass} compact />
                <Time iso={r.createdAt} className="text-xs text-ink-3" />
              </Link>
            </li>
          );
        })}
      </ol>
      {latest > 1 ? (
        <p className="mt-2 flex flex-wrap items-center gap-1.5 text-sm">
          <span className="text-ink-3">Changes since</span>
          {revisions.slice(0, viewing - 1).map((r) => (
            <Link
              key={r.n}
              href={codeHref(repo, id, viewing === latest ? undefined : viewing, r.n)}
              aria-current={since === r.n ? "page" : undefined}
              className={cx("inline-flex min-h-6 items-center rounded-sm px-1.5 hover:bg-hover", since === r.n ? "bg-accent-soft text-accent-ink" : "text-ink-2")}
            >
              r{r.n}
            </Link>
          ))}
          {viewing === 1 ? <span className="text-ink-3">nothing earlier</span> : null}
        </p>
      ) : null}
    </>
  );
}
