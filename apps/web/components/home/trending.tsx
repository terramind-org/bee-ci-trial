/** Trending repos as compact repo cards, shared by the dashboard (5) and the instance landing (6). */
import type { TrendingRepo } from "@/lib/api/types";
import { RepoCard } from "@/components/repo-card";
import { visibilityOf } from "@/components/repo/query";

export function TrendingCards({ items, className }: { items: TrendingRepo[]; className?: string }) {
  const hits = items.filter((t) => t.repo);
  if (!hits.length) return <p className="rounded-lg border border-line px-3.5 py-6 text-center text-sm text-ink-3">No repositories are trending yet.</p>;
  return (
    <ul className={className ?? "flex flex-col gap-2"}>
      {hits.map((t) => {
        const r = t.repo!;
        return (
          <li key={r.fullName} className="min-w-0">
            <RepoCard
              className="h-full"
              fullName={r.fullName}
              description={r.description}
              language={r.primaryLanguage}
              stars={Number(r.starCount)}
              updatedAt={r.pushedAt ?? r.updatedAt}
              visibility={visibilityOf(r.visibility)}
            />
          </li>
        );
      })}
    </ul>
  );
}
