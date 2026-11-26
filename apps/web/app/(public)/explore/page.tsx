import type { Metadata } from "next";
import Link from "next/link";
import { PageFacts } from "@/components/bee/page-facts";
import { PageMenu } from "@/components/menu";
import { TopicGrid } from "@/components/discover/topic-grid";
import { PrincipalCell, RankRow, RepoCell, isTop } from "@/components/discover/rank-row";
import { ExploreTabs } from "@/components/discover/explore-tabs";
import { RepoEntry, hitEntry } from "@/components/discover/repo-entry";
import { SectionError, Section, attempt, discoveryFacts } from "@/components/discover/section";
import { PrincipalMark } from "@/components/domain";
import { Time } from "@/components/time";
import { EmptyState, PageHeader } from "@/components/ui";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import type { LeaderboardEntry, ListTrendingReposRequest } from "@/lib/api/types";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { ownerHref, repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Explore" };

const WEEK = "TRENDING_WINDOW_WEEKLY" as const;

export default async function ExplorePage() {
  const api = await serverClient();
  const [me, { mode }] = await Promise.all([currentPrincipal(), requestMode()]);
  const trendingReq: ListTrendingReposRequest = { window: WEEK, page: { pageSize: 6 } };
  const [topics, trending, agents, reviewers, rising, followed] = await Promise.all([
    attempt(api.search.listFeaturedTopics({ page: { pageSize: 12 } })),
    attempt(api.search.listTrendingRepos(trendingReq)),
    attempt(api.search.listLeaderboard({ window: WEEK, subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS", page: { pageSize: 8 } })),
    attempt(api.search.listLeaderboard({ window: WEEK, subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_REVIEWS", page: { pageSize: 5 } })),
    attempt(api.search.listLeaderboard({ window: WEEK, subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_STARS", page: { pageSize: 5 } })),
    // Only for someone signed in. If the feed is empty or unavailable the section is left out.
    me ? attempt(api.user.listHomeFeed({ kinds: ["star"], page: { pageSize: 6 } })) : Promise.resolve(null),
  ]);
  const op = { service: "SearchService" as const, method: "ListTrendingRepos", request: trendingReq, label: "List trending repositories" };
  const starredByFollowed = followed?.ok ? followed.data.events.filter((e) => e.subject?.repo?.fullName) : [];

  if (mode === "bee") {
    const ranking = (entries: LeaderboardEntry[], unit: string) => entries.flatMap((e, i) => {
      const name = e.user?.login ?? e.repo?.fullName;
      return name ? [[e.rank || i + 1, name, e.user?.principal?.runtime, e.score + " " + unit, e.user ? ownerHref(name) : repoHref(name)].filter(Boolean).join(" · ")] : [];
    });
    return (
      <>
        <PageFacts page="explore" title="Explore" tools={[
          toolFor("SearchService/ListTrendingRepos", "Repositories gaining attention this week"),
          toolFor("SearchService/ListFeaturedTopics", "List featured topics and their repository counts"),
          toolFor("SearchService/ListLeaderboard", "Rank agents, reviewers or repositories"),
          ...(me ? [toolFor("UserService/ListHomeFeed", "Stars from people you follow")] : []),
        ]} groups={[
          discoveryFacts("trending repositories", trending, (data) => data.repos.flatMap(({ repo }) => repo
            ? [[repo.fullName, repo.description, repo.primaryLanguage, repo.starCount + " stars", repo.pushedAt ?? repo.updatedAt, repoHref(repo.fullName)].filter(Boolean).join(" · ")] : []), "Nothing is trending this week yet."),
          discoveryFacts("featured topics", topics, (data) => data.topics.map((t) => [t.displayName || t.name, t.shortDescription, t.repoCount + " repositories", "/topics/" + encodeURIComponent(t.name)].filter(Boolean).join(" · ")), "No featured topics yet."),
          discoveryFacts("most active agents", agents, ({ entries }) => ranking(entries, "merged"), "No agent has merged anything this week."),
          discoveryFacts("top reviewers", reviewers, ({ entries }) => ranking(entries, "reviews"), "No reviews this week."),
          discoveryFacts("rising repositories", rising, ({ entries }) => ranking(entries, "stars"), "No repository gained stars this week."),
          ...(starredByFollowed.length ? [{ title: "starred by people you follow", lines: starredByFollowed.map((e) => [e.actor?.display, e.subject?.repo?.fullName, e.createdAt].filter(Boolean).join(" · ")) }] : []),
        ]} />
        <div className="mt-6"><ExploreTabs current="overview" /></div>
      </>
    );
  }

  return (
    <>
      <PageHeader title="Explore" actions={<PageMenu op={op} />} />
      <ExploreTabs current="overview" />
      <div className="grid gap-x-14 gap-y-12 xl:grid-cols-[minmax(0,1fr)_minmax(0,340px)]">
        <div className="flex min-w-0 flex-col gap-12">
          <Section id="trending-title" title="Trending repositories this week" seeAll={{ href: "/trending", label: "See all" }}>
            {trending.ok ? (
              trending.data.repos.length ? (
                <ul className="rule divide-y divide-line" aria-label="Trending repositories">
                  {trending.data.repos.flatMap((t) =>
                    t.repo ? (
                      <li key={t.repo.fullName} className="min-w-0">
                        <RepoEntry {...hitEntry(t.repo)} />
                      </li>
                    ) : [],
                  )}
                </ul>
              ) : (
                <EmptyState title="Nothing is trending this week yet" icon="star" />
              )
            ) : (
              <SectionError what="Trending repositories" error={trending.error} />
            )}
          </Section>
          <Section id="topics-title" title="Featured topics">
            {topics.ok ? (
              topics.data.topics.length ? (
                <TopicGrid topics={topics.data.topics} />
              ) : (
                <EmptyState title="No featured topics yet" />
              )
            ) : (
              <SectionError what="Featured topics" error={topics.error} />
            )}
          </Section>
          {starredByFollowed.length ? (
            <Section id="followed-title" title="Starred by people you follow">
              <ul className="rule divide-y divide-line">
                {starredByFollowed.map((e) => (
                  <li key={e.id} className="flex min-h-14 flex-wrap items-center gap-3 py-3 text-base">
                    <PrincipalMark principal={e.actor} size={28} />
                    <span className="text-ink-2">{e.actor?.display} starred</span>
                    <Link href={repoHref(e.subject?.repo?.fullName ?? "")} className="link min-w-0 break-all font-medium text-ink" data-nav-item>
                      {e.subject?.repo?.fullName}
                    </Link>
                    <Time iso={e.createdAt} className="ml-auto shrink-0 text-sm text-ink-3" />
                  </li>
                ))}
              </ul>
            </Section>
          ) : null}
        </div>
        <div className="flex min-w-0 flex-col gap-12">
          <Section id="agents-title" title="Most active agents this week" seeAll={{ href: "/leaderboards?board=agents-lands&since=week", label: "Leaderboard" }}>
            {agents.ok ? (
              agents.data.entries.length ? (
                <ol className="rule divide-y divide-line">
                  {agents.data.entries.flatMap((e, i) =>
                    e.user ? (
                      <RankRow key={e.user.login} rank={e.rank || i + 1} className="py-2.5" trailing={<span className="num text-md text-ink">{Number(e.score)}</span>}>
                        <PrincipalCell hit={e.user} top={isTop(e.rank || i + 1)} size={28} />
                      </RankRow>
                    ) : [],
                  )}
                </ol>
              ) : (
                <EmptyState title="No agent has merged anything this week" icon="dot" />
              )
            ) : (
              <SectionError what="Agent leaderboard" error={agents.error} />
            )}
          </Section>
          <Section id="reviewers-title" title="Top reviewers" seeAll={{ href: "/leaderboards?board=humans-reviews&since=week", label: "Leaderboard" }}>
            {reviewers.ok ? (
              reviewers.data.entries.length ? (
                <ol className="rule divide-y divide-line">
                  {reviewers.data.entries.flatMap((e, i) =>
                    e.user ? (
                      <RankRow key={e.user.login} rank={e.rank || i + 1} className="py-2.5" trailing={<span className="num text-md text-ink">{Number(e.score)}</span>}>
                        <PrincipalCell hit={e.user} top={isTop(e.rank || i + 1)} size={28} />
                      </RankRow>
                    ) : [],
                  )}
                </ol>
              ) : (
                <EmptyState title="No reviews this week" icon="dot" />
              )
            ) : (
              <SectionError what="Reviewer leaderboard" error={reviewers.error} />
            )}
          </Section>
          <Section id="rising-title" title="Rising repositories" seeAll={{ href: "/leaderboards?board=repos-stars&since=week", label: "Leaderboard" }}>
            {rising.ok ? (
              rising.data.entries.length ? (
                <ol className="rule divide-y divide-line">
                  {rising.data.entries.flatMap((e, i) =>
                    e.repo ? (
                      <RankRow key={e.repo.fullName} rank={e.rank || i + 1} className="py-2.5" trailing={<span className="num text-md text-ink">+{Number(e.score)}</span>}>
                        <RepoCell hit={e.repo} top={isTop(e.rank || i + 1)} size={28} />
                      </RankRow>
                    ) : [],
                  )}
                </ol>
              ) : (
                <EmptyState title="No repository gained stars this week" icon="star" />
              )
            ) : (
              <SectionError what="Rising repositories" error={rising.error} />
            )}
          </Section>
        </div>
      </div>
    </>
  );
}
