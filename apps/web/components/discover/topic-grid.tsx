import Link from "next/link";
import { cx } from "@/components/ui";
import type { Topic } from "@/lib/api/types";
import { compact } from "@/lib/format";

/** Topics as a plain list in columns: the name, one line of description, and how many repos carry it. */
export function TopicGrid({ topics, className }: { topics: Topic[]; className?: string }) {
  return (
    <ul className={cx("grid gap-x-10 gap-y-6 sm:grid-cols-2", className)} aria-label="Topics">
      {topics.map((t) => (
        <li key={t.name} className="flex min-w-0 flex-col">
          <Link href={"/topics/" + encodeURIComponent(t.name)} className="link inline-flex min-h-6 min-w-0 items-center break-all text-lg font-medium text-ink" data-nav-item>
            {t.displayName || t.name}
          </Link>
          {t.shortDescription ? <p className="line-clamp-2 text-base text-ink-2">{t.shortDescription}</p> : null}
          <p className="mt-0.5 text-sm text-ink-3">
            <span className="num">{compact(Number(t.repoCount))}</span> {Number(t.repoCount) === 1 ? "repository" : "repositories"}
          </p>
        </li>
      ))}
    </ul>
  );
}
