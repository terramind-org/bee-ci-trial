/** The repo card (docs/design/tokens.md, "Repo card"): shared by the repo list, discovery and home. */
import Link from "next/link";
import type { Visibility } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { Icon } from "./icons";
import { Time } from "./time";
import { Badge, cx } from "./ui";

export interface RepoCardProps {
  fullName: string;
  description?: string;
  language?: string;
  stars: number;
  /** RFC 3339. */
  updatedAt?: string;
  visibility: Visibility;
  topics?: string[];
  className?: string;
}

/** Public is an outline badge, private and internal are neutral; the word is always printed. */
export function VisibilityBadge({ visibility }: { visibility: Visibility }) {
  if (visibility === "VISIBILITY_PUBLIC") return <Badge tone="outline">Public</Badge>;
  if (visibility === "VISIBILITY_INTERNAL") return <Badge>Internal</Badge>;
  return <Badge>Private</Badge>;
}

export function RepoCard({ fullName, description, language, stars, updatedAt, visibility, topics, className }: RepoCardProps) {
  return (
    <article className={cx("flex min-w-0 flex-col gap-2 rounded-lg border border-line bg-surface p-3.5 transition-colors duration-[120ms] hover:bg-hover", className)} aria-label={fullName}>
      <div className="flex min-w-0 items-center gap-2">
        <Link href={repoHref(fullName)} className="min-w-0 truncate text-base font-medium text-ink hover:underline" data-nav-item>
          {fullName}
        </Link>
        <VisibilityBadge visibility={visibility} />
      </div>
      {description ? <p className="line-clamp-2 text-sm text-ink-2">{description}</p> : null}
      {topics?.length ? (
        <ul className="flex flex-wrap gap-1" aria-label="Topics">
          {topics.map((t) => (
            <li key={t}>
              <Link href={"/topics/" + encodeURIComponent(t)} className="inline-flex h-5 items-center rounded-[5px] bg-accent-soft px-1.5 text-xs font-medium text-accent-ink hover:underline">
                {t}
              </Link>
            </li>
          ))}
        </ul>
      ) : null}
      <div className="mt-auto flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-ink-3">
        {language ? (
          <span className="inline-flex items-center gap-1.5">
            <span aria-hidden="true" className="size-2 rounded-full bg-ink-3" />
            {language}
          </span>
        ) : null}
        {/* role="img": an aria-label is only permitted on an element with a role (axe aria-prohibited-attr). */}
        <span role="img" className="inline-flex items-center gap-1" aria-label={stars === 1 ? "1 star" : stars + " stars"}>
          <Icon name="star" size={12} />
          <span className="num" aria-hidden="true">
            {compact(stars)}
          </span>
        </span>
        {updatedAt ? (
          <span>
            Updated <Time iso={updatedAt} />
          </span>
        ) : null}
      </div>
    </article>
  );
}
