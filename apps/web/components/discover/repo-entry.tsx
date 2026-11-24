/**
 * Repositories as large reading entries (no cards): the owner's mark, the name, a two-line
 * description, then one quiet line of language, stars and recency. Entries are separated by a
 * hairline. Used by /repositories, topics, profiles and Explore.
 */
import Link from "next/link";
import type { ReactNode } from "react";
import { Icon } from "@/components/icons";
import { Time } from "@/components/time";
import { EmptyState, buttonClass, cx } from "@/components/ui";
import { visibilityOf } from "@/components/repo/query";
import type { SearchRepoHit, StarredRepo, Visibility } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { OwnerMark, RepoName } from "./rank-row";

export interface RepoEntryProps {
  fullName: string;
  description?: string;
  language?: string;
  stars: number;
  /** RFC 3339. */
  updatedAt?: string;
  /** Prefix for the recency: "Updated" for a repo, "Starred" for a star. */
  when?: string;
  visibility?: Visibility;
  topics?: string[];
  trailing?: ReactNode;
  className?: string;
}

const VISIBILITY_WORD: Record<Visibility, string> = { VISIBILITY_PUBLIC: "Public", VISIBILITY_INTERNAL: "Internal", VISIBILITY_PRIVATE: "Private", VISIBILITY_UNSPECIFIED: "Private" };

export function RepoEntry({ fullName, description, language, stars, updatedAt, when = "Updated", visibility, topics, trailing, className }: RepoEntryProps) {
  return (
    <article className={cx("flex items-start gap-4 py-5", className)} aria-label={fullName}>
      <OwnerMark fullName={fullName} size={36} />
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-center gap-3">
          <RepoName fullName={fullName} className="text-lg" />
          {visibility && visibility !== "VISIBILITY_PUBLIC" ? <span className="shrink-0 text-sm text-ink-3">{VISIBILITY_WORD[visibility]}</span> : null}
        </div>
        {description ? <p className="mt-1 line-clamp-2 max-w-2xl text-md text-ink-2">{description}</p> : null}
        <div className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1 text-sm text-ink-3">
          {language ? (
            <span className="inline-flex items-center gap-1.5">
              <span aria-hidden="true" className="size-2 rounded-full bg-ink-3" />
              {language}
            </span>
          ) : null}
          {/* role="img": an aria-label is only permitted on an element with a role (axe aria-prohibited-attr). */}
          <span role="img" className="inline-flex items-center gap-1" aria-label={stars === 1 ? "1 star" : stars + " stars"}>
            <Icon name="star" size={14} />
            <span className="num" aria-hidden="true">
              {compact(stars)}
            </span>
          </span>
          {updatedAt ? (
            <span>
              {when} <Time iso={updatedAt} />
            </span>
          ) : null}
        </div>
        {topics?.length ? (
          <ul className="mt-2 flex flex-wrap gap-x-4 gap-y-0.5" aria-label="Topics">
            {topics.map((t) => (
              <li key={t}>
                <Link href={"/topics/" + encodeURIComponent(t)} className="inline-flex h-6 items-center text-sm text-accent-ink hover:underline">
                  {t}
                </Link>
              </li>
            ))}
          </ul>
        ) : null}
      </div>
      {trailing ? <div className="shrink-0 self-center">{trailing}</div> : null}
    </article>
  );
}

export function hitEntry(h: SearchRepoHit): RepoEntryProps {
  return {
    fullName: h.fullName,
    description: h.description || undefined,
    language: h.primaryLanguage || undefined,
    stars: Number(h.starCount),
    updatedAt: h.pushedAt ?? h.updatedAt,
    visibility: visibilityOf(h.visibility),
    topics: h.topics.length ? h.topics : undefined,
  };
}

export function RepoEntries({ hits, empty }: { hits: SearchRepoHit[]; empty: ReactNode }) {
  if (!hits.length) return <>{empty}</>;
  return (
    <ul className="rule divide-y divide-line" aria-label="Repositories">
      {hits.map((h) => (
        <li key={h.fullName} className="min-w-0">
          <RepoEntry {...hitEntry(h)} />
        </li>
      ))}
    </ul>
  );
}

/** Starred repos carry no visibility or topics, so they read as plain entries. */
export function StarredEntries({ repos, empty }: { repos: StarredRepo[]; empty: ReactNode }) {
  if (!repos.length) return <>{empty}</>;
  return (
    <ul className="rule divide-y divide-line" aria-label="Starred repositories">
      {repos.map((r) => (
        <li key={r.fullName} className="min-w-0">
          <RepoEntry fullName={r.fullName} description={r.description || undefined} language={r.language || undefined} stars={Number(r.starCount)} updatedAt={r.starredAt} when="Starred" />
        </li>
      ))}
    </ul>
  );
}

export function NoRepoResults({ filtered, clearHref, children }: { filtered: boolean; clearHref: string; children?: ReactNode }) {
  return (
    <EmptyState
      title={filtered ? "No repositories match these filters" : "Nothing here yet"}
      icon="repo"
      action={
        filtered ? (
          <Link href={clearHref} className={buttonClass("secondary", "sm")}>
            Clear filters
          </Link>
        ) : undefined
      }
    >
      {filtered ? "Try a different word, or drop a filter." : children}
    </EmptyState>
  );
}

/** Newer/older links; `first` is shown once the reader is past the first page. */
export function PageNav({ first, next, label }: { first?: string; next?: string; label: string }) {
  if (!first && !next) return null;
  return (
    <nav aria-label={label} className="mt-6 flex items-center justify-between gap-2">
      {first ? (
        <Link href={first} className={buttonClass("secondary", "sm")}>
          First page
        </Link>
      ) : (
        <span />
      )}
      {next ? (
        <Link href={next} rel="next" className={buttonClass("secondary", "sm")}>
          Next page
        </Link>
      ) : null}
    </nav>
  );
}
