import Link from "next/link";
import { PrincipalMark } from "@/components/domain";
import { FollowButton } from "@/components/follow-button";
import { Icon } from "@/components/icons";
import { StarButton } from "@/components/star-button";
import { EmptyState } from "@/components/ui";
import type { TrendingDeveloper, TrendingRepo } from "@/lib/api/types";
import { compact } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { SINCE_LABEL, type Since } from "./params";
import { OwnerMark, PrincipalCell, RankRow, RepoName, isTop } from "./rank-row";

const IN_WINDOW: Record<Since, string> = { day: "today", week: "this week", month: "this month" };

/** What the viewer already starred or follows, so the buttons start in the right state. */
export interface Relations {
  signedIn: boolean;
  /** Full names of repos the viewer starred. */
  starred: readonly string[];
  /** Logins the viewer follows. */
  following: readonly string[];
  /** The viewer's principal id: nobody follows themselves. */
  meId?: string;
}

export function TrendingRepoList({ repos, since, relations, firstRank = 1 }: { repos: TrendingRepo[]; since: Since; relations: Relations; firstRank?: number }) {
  if (repos.length === 0) return <EmptyState title={"Nothing is trending " + IN_WINDOW[since] + " yet"} icon="star" />;
  return (
    <ol className="rule divide-y divide-line" aria-label={"Trending repositories, " + SINCE_LABEL[since].toLowerCase()}>
      {repos.map((t, i) => {
        const hit = t.repo;
        if (!hit) return null;
        const rank = t.rank || firstRank + i;
        return (
          <RankRow
            key={hit.fullName}
            rank={rank}
            className="py-5"
            avatar={<OwnerMark fullName={hit.fullName} size={36} />}
            trailing={<StarButton starred={relations.starred.includes(hit.fullName)} count={Number(hit.starCount)} target={hit.fullName} signedIn={relations.signedIn} copyAs={false} />}
          >
            <RepoName fullName={hit.fullName} top={isTop(rank)} className="block text-lg" />
            {hit.description ? <p className="mt-1 line-clamp-2 max-w-2xl text-md text-ink-2">{hit.description}</p> : null}
            <p className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1 text-sm text-ink-3">
              {hit.primaryLanguage ? <span>{hit.primaryLanguage}</span> : null}
              <span className="inline-flex items-center gap-1">
                <Icon name="star" size={14} />
                <span className="num">+{compact(Number(t.starsInWindow))}</span> {IN_WINDOW[since]}
              </span>
              {t.builtBy.length ? (
                <span className="inline-flex items-center flex-wrap gap-2">
                  Built by
                  <span className="inline-flex gap-1">
                    {t.builtBy.map((p) => (
                      <PrincipalMark key={p.id} principal={p} size={24} />
                    ))}
                  </span>
                  {t.builtBy
                    .filter((p) => p.kind === "PRINCIPAL_KIND_AGENT" && p.runtime)
                    .slice(0, 1)
                    .map((p) => (
                      <span key={p.id}>{p.runtime}</span>
                    ))}
                </span>
              ) : null}
            </p>
          </RankRow>
        );
      })}
    </ol>
  );
}

export function TrendingDeveloperList({ developers, kind, since, relations, firstRank = 1 }: { developers: TrendingDeveloper[]; kind: "developers" | "agents"; since: Since; relations: Relations; firstRank?: number }) {
  if (developers.length === 0) return <EmptyState title={"No " + kind + " are trending " + IN_WINDOW[since] + " yet"} icon="user" />;
  return (
    <ol className="rule divide-y divide-line" aria-label={"Trending " + kind + ", " + SINCE_LABEL[since].toLowerCase()}>
      {developers.map((d, i) => {
        const user = d.user;
        if (!user) return null;
        const rank = d.rank || firstRank + i;
        const self = !!relations.meId && user.principal?.id === relations.meId;
        return (
          <RankRow
            key={user.login}
            rank={rank}
            className="py-4"
            trailing={self ? null : <FollowButton following={relations.following.includes(user.login)} target={user.login} signedIn={relations.signedIn} copyAs={false} />}
          >
            <PrincipalCell hit={user} top={isTop(rank)} />
            {d.popularRepo || user.bio ? (
              <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-4 gap-y-0.5 pl-12 text-sm text-ink-3">
                {user.bio ? <span className="min-w-0 truncate">{user.bio}</span> : null}
                {d.popularRepo ? (
                  <span className="inline-flex min-w-0 items-center gap-1.5">
                    Popular
                    <Link href={repoHref(d.popularRepo.fullName)} className="link inline-flex min-h-6 min-w-0 items-center break-all text-ink-2">
                      {d.popularRepo.fullName}
                    </Link>
                  </span>
                ) : null}
              </p>
            ) : null}
          </RankRow>
        );
      })}
    </ol>
  );
}
