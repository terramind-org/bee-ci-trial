import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { PageMenu } from "@/components/menu";
import { ExploreTabs } from "@/components/discover/explore-tabs";
import { Pager, href } from "@/components/discover/pager";
import { SINCE_LABEL, SINCE_WINDOW, parseChoice, type Since } from "@/components/discover/params";
import { SectionError, attempt, discoveryFacts } from "@/components/discover/section";
import { TrendingDeveloperList, TrendingRepoList, type Relations } from "@/components/discover/trending-list";
import { Segments, WindowSwitch } from "@/components/discover/window-switch";
import { Button, Input, PageHeader } from "@/components/ui";
import type { ListTrendingDevelopersResponse, ListTrendingReposResponse } from "@/lib/api/types";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { ownerHref, repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Trending" };

const KINDS = ["repos", "developers", "agents"] as const;
type Kind = (typeof KINDS)[number];
const KIND_LABEL: Record<Kind, string> = { repos: "Repositories", developers: "Developers", agents: "Agents" };
const SINCES: readonly Since[] = ["day", "week", "month"];
const PAGE = 25;

export default async function TrendingPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const kind = parseChoice(param(sp, "kind"), KINDS, "repos");
  const since = parseChoice(param(sp, "since"), SINCES, "week");
  const language = param(sp, "language").trim();
  const after = param(sp, "after") || undefined;
  const here = (over: Record<string, string | undefined>) => href("/trending", { kind: kind === "repos" ? undefined : kind, since: since === "week" ? undefined : since, language: language || undefined, ...over });

  return guard(async () => {
    const api = await serverClient();
    const [me, { mode }] = await Promise.all([currentPrincipal(), requestMode()]);
    const viewer = me && kind !== "repos" && mode === "keeper" ? await attempt(api.user.getViewer({})) : null;
    const window = SINCE_WINDOW[since];
    const page = { pageSize: PAGE, ...(after ? { pageToken: after } : {}) };

    const repoReq = { window, ...(language ? { language } : {}), page };
    const devReq = { window, ...(language ? { language } : {}), includeAgents: kind === "agents", page };
    // The suggestions are the languages of what is trending in this window across all languages, so
    // choosing one never removes the others. The field also takes any language the server knows.
    const [main, languages, starred, following] = await Promise.all([
      kind === "repos" ? attempt(api.search.listTrendingRepos(repoReq)) : attempt(api.search.listTrendingDevelopers(devReq)),
      attempt(api.search.listTrendingRepos({ window, page: { pageSize: 100 } })),
      me && kind === "repos" && mode === "keeper" ? attempt(api.user.listStarred({ login: "", page: { pageSize: 200 } })) : Promise.resolve(null),
      viewer?.ok ? attempt(api.user.listFollowing({ login: viewer.data.login, page: { pageSize: 200 } })) : Promise.resolve(null),
    ]);
    const relations: Relations = {
      signedIn: !!me,
      meId: me?.id,
      starred: starred?.ok ? starred.data.repos.map((r) => r.fullName) : [],
      following: following?.ok ? following.data.users.map((u) => u.login) : [],
    };
    const langs = [...new Set([...(languages.ok ? languages.data.repos.map((t) => t.repo?.primaryLanguage ?? "") : []), language].filter(Boolean))].sort((a, b) => a.localeCompare(b));
    const op =
      kind === "repos"
        ? { service: "SearchService" as const, method: "ListTrendingRepos", request: repoReq, label: "List trending repositories" }
        : { service: "SearchService" as const, method: "ListTrendingDevelopers", request: devReq, label: kind === "agents" ? "List trending agents" : "List trending developers" };
    const next = main.ok && main.data.page?.nextPageToken ? here({ after: main.data.page.nextPageToken }) : null;

    const filterKinds = <Segments label="Kind" current={kind} segments={KINDS.map((k) => ({ value: k, label: KIND_LABEL[k], href: here({ kind: k === "repos" ? undefined : k, after: undefined }) }))} />;
    const filterWindow = <WindowSwitch current={since} options={SINCES} hrefFor={(s) => here({ since: s === "week" ? undefined : s, after: undefined })} />;
    if (mode === "bee") {
      return (
        <>
          <PageFacts page="trending" title="Trending" tools={[
            toolFor(kind === "repos" ? "SearchService/ListTrendingRepos" : "SearchService/ListTrendingDevelopers", "Read the rankings for this time window"),
            ...(me ? kind === "repos" ? [
              toolFor("UserService/StarRepo", "Star a repository"),
              toolFor("UserService/UnstarRepo", "Remove your star"),
            ] : [
              toolFor("UserService/FollowUser", "Follow a person or agent"),
              toolFor("UserService/UnfollowUser", "Stop following a person or agent"),
            ] : []),
          ]} groups={[
            { title: "filters", facts: [
              { label: "kind", value: KIND_LABEL[kind] }, { label: "window", value: SINCE_LABEL[since] },
              language ? { label: "language", value: language } : null,
            ] },
            discoveryFacts<ListTrendingReposResponse | ListTrendingDevelopersResponse>("rankings", main, (data) => "repos" in data
              ? data.repos.flatMap((t, i) => t.repo ? [[t.rank || i + 1, t.repo.fullName, t.repo.description, t.repo.primaryLanguage, t.starsInWindow + " new stars", t.repo.starCount + " stars total", t.builtBy.map((p) => [p.display, p.runtime].filter(Boolean).join(" · ")).join(", "), repoHref(t.repo.fullName)].filter(Boolean).join(" · ")] : [])
              : data.developers.flatMap((d, i) => d.user ? [[d.rank || i + 1, d.user.login, d.user.principal?.runtime, d.user.bio, d.popularRepo?.fullName, ownerHref(d.user.login)].filter(Boolean).join(" · ")] : []),
            "Nothing is trending in this time window yet."),
          ]} />
          <div className="mt-6"><ExploreTabs current="trending" />{filterKinds}{filterWindow}<Pager nextHref={next} /></div>
        </>
      );
    }

    return (
      <>
        <PageHeader title="Trending" actions={<PageMenu op={op} />} />
        <ExploreTabs current="trending" />
        <div className="mb-6 flex flex-wrap items-center gap-x-8 gap-y-3">
          {filterKinds}
          {filterWindow}
          <form action="/trending" method="get" className="flex max-w-full flex-wrap items-center gap-2">
            {kind !== "repos" ? <input type="hidden" name="kind" value={kind} /> : null}
            {since !== "week" ? <input type="hidden" name="since" value={since} /> : null}
            <label htmlFor="language" className="sr-only">
              Language
            </label>
            <Input id="language" name="language" defaultValue={language} list="trending-languages" placeholder="Any language" autoComplete="off" className="h-[34px] w-36 text-sm" />
            <datalist id="trending-languages">
              {langs.map((l) => (
                <option key={l} value={l} />
              ))}
            </datalist>
            <Button type="submit" size="sm">
              Apply
            </Button>
          </form>
        </div>
        <div>
          {!main.ok ? (
            <SectionError what="Trending" error={main.error} />
          ) : "repos" in main.data ? (
            <TrendingRepoList repos={main.data.repos} since={since} relations={relations} />
          ) : (
            <TrendingDeveloperList developers={main.data.developers} kind={kind === "agents" ? "agents" : "developers"} since={since} relations={relations} />
          )}
          <Pager nextHref={next} />
        </div>
      </>
    );
  });
}
