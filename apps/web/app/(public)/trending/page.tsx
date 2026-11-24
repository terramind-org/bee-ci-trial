import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { ExploreTabs } from "@/components/discover/explore-tabs";
import { Pager, href } from "@/components/discover/pager";
import { SINCE_LABEL, SINCE_WINDOW, parseChoice, type Since } from "@/components/discover/params";
import { SectionError, attempt } from "@/components/discover/section";
import { TrendingDeveloperList, TrendingRepoList, type Relations } from "@/components/discover/trending-list";
import { Segments, WindowSwitch } from "@/components/discover/window-switch";
import { Button, Input, PageHeader } from "@/components/ui";
import { currentPrincipal, serverClient } from "@/lib/api/server";
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
    const me = await currentPrincipal();
    const window = SINCE_WINDOW[since];
    const page = { pageSize: PAGE, ...(after ? { pageToken: after } : {}) };

    const repoReq = { window, ...(language ? { language } : {}), page };
    const devReq = { window, ...(language ? { language } : {}), includeAgents: kind === "agents", page };
    // The suggestions are the languages of what is trending in this window across all languages, so
    // choosing one never removes the others. The field also takes any language the server knows.
    const [main, languages, starred, following] = await Promise.all([
      kind === "repos" ? attempt(api.search.listTrendingRepos(repoReq)) : attempt(api.search.listTrendingDevelopers(devReq)),
      attempt(api.search.listTrendingRepos({ window, page: { pageSize: 100 } })),
      me ? attempt(api.user.listStarred({ login: me.display, page: { pageSize: 200 } })) : Promise.resolve(null),
      me ? attempt(api.user.listFollowing({ login: me.display, page: { pageSize: 200 } })) : Promise.resolve(null),
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

    return (
      <>
        <PageOp op={op} />
        <PageHeader title="Trending" lede={"What is gaining attention on Beehive, " + SINCE_LABEL[since].toLowerCase() + "."} actions={<CopyAs op={op} />} />
        <ExploreTabs current="trending" />
        <div className="mb-6 flex flex-wrap items-center gap-x-8 gap-y-3">
          <Segments label="Kind" current={kind} segments={KINDS.map((k) => ({ value: k, label: KIND_LABEL[k], href: here({ kind: k === "repos" ? undefined : k, after: undefined }) }))} />
          <WindowSwitch current={since} options={SINCES} hrefFor={(s) => here({ since: s === "week" ? undefined : s, after: undefined })} />
          <form action="/trending" method="get" className="flex items-center gap-2">
            {kind !== "repos" ? <input type="hidden" name="kind" value={kind} /> : null}
            {since !== "week" ? <input type="hidden" name="since" value={since} /> : null}
            <label htmlFor="language" className="text-base text-ink-2">
              Language
            </label>
            <Input id="language" name="language" defaultValue={language} list="trending-languages" placeholder="Any language" autoComplete="off" className="w-40" />
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
          ) : kind === "repos" ? (
            <TrendingRepoList repos={(main.data as { repos: never[] }).repos} since={since} relations={relations} />
          ) : (
            <TrendingDeveloperList developers={(main.data as { developers: never[] }).developers} kind={kind} since={since} relations={relations} />
          )}
          <Pager nextHref={next} />
        </div>
      </>
    );
  });
}
