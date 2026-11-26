import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { Unavailable } from "@/components/error-state";
import { NoRepoResults, PageNav } from "@/components/discover/repo-entry";
import { PageMenu } from "@/components/menu";
import { RepoFilters } from "@/components/repo/repo-filters";
import { collect, searchPage } from "@/components/repo/search-hits";
import { COMMON_LANGUAGES, MEMORY_LIMIT, PAGE_SIZE, REPO_TABS, hitOfRepo, listHref, parseListParams, refine, searchQuery, serverSort, visibilityOf } from "@/components/repo/query";
import { ButtonLink, PageHeader, TabLinks } from "@/components/ui";
import type { BeeClient } from "@/lib/api/client";
import { mapLimit } from "@/lib/api/ops/shared";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { loadNeeds, requestNow } from "@/lib/inbox/load";
import { guard, type SearchParams } from "@/lib/page";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../request-mode";
import { RepositoryList, repositoryLine, type RepositoryRow } from "./repository-list";

export const metadata: Metadata = { title: "Repositories" };

async function withActiveAgents(api: BeeClient, row: RepositoryRow, now: number): Promise<RepositoryRow> {
  try {
    const { leases } = await api.lease.listLeases({ repo: { fullName: row.fullName } });
    if (leases.some((lease) => !lease.expiresAt || !Number.isFinite(Date.parse(lease.expiresAt)))) return row;
    const active = leases.filter((lease) => Date.parse(lease.expiresAt ?? "") > now);
    // Some callers receive holder IDs without their kinds. An incomplete count would mislead.
    if (active.some(({ holder }) => !holder?.kind || holder.kind === "PRINCIPAL_KIND_UNSPECIFIED")) return row;
    const agents = new Set(active.filter(({ holder }) => holder?.kind === "PRINCIPAL_KIND_AGENT").map(({ holder }) => holder?.id));
    return { ...row, agents: agents.size };
  } catch {
    return row;
  }
}

export default async function RepositoriesPage({ searchParams }: { searchParams: SearchParams }) {
  const [sp, me, { mode }] = await Promise.all([searchParams, currentPrincipal(), requestMode()]);
  const signedIn = !!me;
  const p = parseListParams(sp, signedIn);
  const filtered = !!(p.q || p.visibility || p.language || p.topic);
  const clear = listHref({ q: "", visibility: "", language: "", topic: "", cursor: "", page: 1 }, p, signedIn);

  return guard(async () => {
    const api = await serverClient();
    // This promise is shared with the rail; only show a per-repo count from a complete scan.
    const needScan = me?.kind === "PRINCIPAL_KIND_HUMAN" ? loadNeeds().catch(() => undefined) : undefined;
    // Personal forks are indexed by account login, which can differ from the person's display name.
    const viewer = p.tab === "yours" && me ? await api.user.getViewer({}).catch(() => undefined) : undefined;
    const query = searchQuery(p, viewer?.login || me?.display || "");
    const from = (p.page - 1) * PAGE_SIZE;
    let rows: RepositoryRow[] = [];
    let total: number | undefined;
    let languages: string[] = [];
    let next: string | undefined;
    let limited = "";
    let unavailable: BeeError | undefined;
    let op: React.ComponentProps<typeof PageMenu>["op"] = {
      service: "SearchService", method: "SearchRepos",
      request: { query, sort: serverSort(p.sort), page: { pageSize: PAGE_SIZE, pageToken: p.cursor } },
      label: "Search repositories",
    };
    const numberedNext = (length: number) => from + PAGE_SIZE < length ? listHref({ page: p.page + 1 }, p, signedIn) : undefined;
    const rowOf = (hit: ReturnType<typeof hitOfRepo>): RepositoryRow => ({
      fullName: hit.fullName, description: hit.description, visibility: hit.visibility ? visibilityOf(hit.visibility) : undefined, activity: hit.pushedAt ?? hit.updatedAt,
    });

    try {
      if (p.tab === "public" || p.tab === "yours") {
        const result = await searchPage(api, query, p);
        total = result.total;
        languages = result.languages;
        rows = result.hits.map(rowOf);
        next = result.next ? listHref({ cursor: result.next }, p, signedIn) : result.hasMoreLocal ? listHref({ page: p.page + 1 }, p, signedIn) : undefined;
        if (result.truncated) limited = "Showing the first " + MEMORY_LIMIT + " repositories, ordered by name.";
      } else if (p.tab === "orgs") {
        op = { service: "RepoService", method: "ListMyRepos", request: {}, label: "List your organizations’ repositories" };
        const mine = (await api.repo.listMyRepos({})).repos.map(hitOfRepo);
        languages = mine.map((h) => h.primaryLanguage);
        const matches = refine(mine, p);
        total = matches.length;
        rows = matches.slice(from, from + PAGE_SIZE).map(rowOf);
        next = numberedNext(matches.length);
      } else {
        const request = { login: "", ...(p.language ? { language: p.language } : {}), sort: "updated" };
        op = { service: "UserService", method: "ListStarred", request, label: "List starred repositories" };
        const { items, more } = await collect(async (pageToken) => {
          const result = await api.user.listStarred({ ...request, page: { pageSize: 100, pageToken } });
          return { items: result.repos, next: result.page?.nextPageToken ?? "" };
        }, MEMORY_LIMIT);
        languages = items.map((r) => r.language);
        const q = p.q.toLowerCase();
        const matches = items.filter((r) => !q || r.fullName.toLowerCase().includes(q) || r.description.toLowerCase().includes(q));
        if (p.sort === "name") matches.sort((a, b) => a.fullName.localeCompare(b.fullName));
        if (p.sort === "stars") matches.sort((a, b) => Number(b.starCount) - Number(a.starCount) || a.fullName.localeCompare(b.fullName));
        total = matches.length;
        rows = (await mapLimit(matches.slice(from, from + PAGE_SIZE), 4, async (r) => {
          const repo = await api.repo.getRepo({ repo: { fullName: r.fullName } }).catch((err: unknown) => {
            if (err instanceof BeeError && ["not_found", "permission_denied", "unauthenticated"].includes(err.code)) return null;
            return undefined;
          });
          if (repo === null) return null;
          return { fullName: r.fullName, description: r.description, visibility: repo?.visibility, activity: repo?.pushedAt };
        })).filter((row) => row !== null);
        next = numberedNext(matches.length);
        if (more) limited = "Showing the first " + MEMORY_LIMIT + " starred repositories.";
      }
    } catch (err) {
      if (!(err instanceof BeeError) || err.code !== "unimplemented") throw err;
      unavailable = err;
    }
    const now = requestNow();
    const [activeRows, needs] = await Promise.all([mapLimit(rows, 4, (row) => withActiveAgents(api, row, now)), needScan]);
    const waiting = new Map<string, number>();
    if (needs && !needs.truncated) {
      for (const fullName of needs.checked) {
        if (!needs.failed.includes(fullName)) waiting.set(fullName, 0);
      }
      for (const need of needs.needs) {
        const n = waiting.get(need.repo);
        if (need.kind !== "post" && n !== undefined) waiting.set(need.repo, n + 1);
      }
    }
    rows = activeRows.map((row) => ({ ...row, needsYou: waiting.get(row.fullName) }));
    const first = p.cursor || p.page > 1 ? listHref({ cursor: "", page: 1 }, p, signedIn) : undefined;
    const tabs = REPO_TABS.filter((t) => signedIn || !t.needsSession).map((t) => ({
      href: listHref({
        tab: t.id, cursor: "", page: 1,
        visibility: t.id === "starred" || t.id === "public" ? "" : p.visibility,
        topic: t.id === "starred" ? "" : p.topic,
      }, p, signedIn),
      label: t.label,
      active: t.id === p.tab,
    }));
    const langs = [...new Set([...COMMON_LANGUAGES, ...languages.filter(Boolean)])].sort((a, b) => a.localeCompare(b));
    const empty = p.tab === "starred" ? "Star a repository and it is listed here." :
      p.tab === "yours" ? "Repositories owned by your account show up here. Your organizations’ repositories are under Your orgs." :
      p.tab === "orgs" ? "No repositories in your organizations yet." : "No public repositories yet.";
    const what = p.tab === "starred" ? "Stars" : "Repository list";
    const navigation = signedIn ? <TabLinks label="Repository lists" items={tabs} /> : null;
    const pager = <PageNav label="Pages" first={first} next={next} />;

    if (mode === "bee") {
      const tools = pageTools("repositories").filter((tool) =>
        tool.op === op?.service + "/" + op?.method || tool.op === "LeaseService/ListLeases" ||
        (p.tab === "yours" && tool.op === "UserService/GetViewer") ||
        (needs && ["ChangeService/ListChanges", "ChangeService/EvaluateLanding", "CheckService/ListChecks", "ChangeService/ListReviews"].includes(tool.op)) ||
        (p.tab === "starred" && tool.op === "RepoService/GetRepo") ||
        (signedIn && ["RepoService/CreateRepo", "ImportService/StartImport"].includes(tool.op)));
      return (
        <>
          <PageFacts page="repositories" title="Repositories" tools={tools} groups={[
            { title: "facts", facts: [
              { label: "showing", value: tabs.find((t) => t.active)?.label ?? "Public" },
              p.q ? { label: "filter", value: p.q } : null,
              p.visibility ? { label: "visibility", value: p.visibility } : null,
              p.language ? { label: "language", value: p.language } : null,
              p.topic ? { label: "topic", value: p.topic } : null,
              { label: "sort", value: p.sort },
              total !== undefined && { label: "repositories", value: total },
              limited ? { label: "limit", value: limited } : null,
            ] },
            { title: "repositories", lines: rows.map(repositoryLine), empty: unavailable ? unavailable.reason + " · " + unavailable.message : filtered ? "No repositories match these filters." : empty },
          ]} />
          <div className="mt-6">{navigation}{pager}</div>
        </>
      );
    }

    return (
      <>
        <PageHeader title="Repositories" actions={
          <>
            {signedIn ? <>
              <ButtonLink href="/new" variant="primary">New repository</ButtonLink>
              <ButtonLink href="/import">Import from GitHub</ButtonLink>
            </> : null}
            {op ? <PageMenu op={op} /> : null}
          </>
        } />
        {navigation}
        <div className="mb-5">
          <RepoFilters params={p} signedIn={signedIn} languages={langs} />
        </div>
        {unavailable ? <Unavailable what={what} message={unavailable.message} reason={unavailable.reason} /> :
          rows.length ? <RepositoryList rows={rows} /> : <NoRepoResults filtered={filtered} clearHref={clear}>{empty}</NoRepoResults>}
        {pager}
        {limited ? <p className="mt-2 text-sm text-ink-3">{limited}</p> : null}
      </>
    );
  });
}
