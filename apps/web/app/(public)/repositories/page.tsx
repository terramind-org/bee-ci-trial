import type { Metadata } from "next";
import { PageOp } from "@/components/copy-as";
import { Unavailable } from "@/components/error-state";
import { NoRepoResults as NoRepos, PageNav as Pager, RepoEntries as RepoHits, StarredEntries as StarredRows } from "@/components/discover/repo-entry";
import { RepoFilters } from "@/components/repo/repo-filters";
import { collect, searchPage } from "@/components/repo/search-hits";
import {
  COMMON_LANGUAGES,
  MEMORY_LIMIT,
  PAGE_SIZE,
  REPO_TABS,
  hitOfRepo,
  listHref,
  parseListParams,
  refine,
  searchQuery,
  serverSort,
  type ListParams,
} from "@/components/repo/query";
import { ButtonLink, PageHeader, TabLinks } from "@/components/ui";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { SearchRepoHit, StarredRepo } from "@/lib/api/types";
import { guard, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Repositories" };

export default async function RepositoriesPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const me = await currentPrincipal();
  const signedIn = !!me;
  const p = parseListParams(sp, signedIn);
  const login = me?.display ?? "";
  const filtered = !!(p.q || p.visibility || p.language || p.topic);
  const clear = listHref({ q: "", visibility: "", language: "", topic: "", cursor: "", page: 1 }, p, signedIn);

  return guard(async () => {
    const api = await serverClient();
    const query = searchQuery(p, login);
    let body: React.ReactNode;
    let total: number | undefined;
    let op: React.ComponentProps<typeof PageOp>["op"] = { service: "SearchService", method: "SearchRepos", request: { query, sort: serverSort(p.sort), page: { pageSize: PAGE_SIZE } }, label: "Search repositories" };
    let languages: string[] = [];

    try {
      if (p.tab === "public" || p.tab === "yours") {
        const r = await searchPage(api, query, p);
        total = r.total;
        languages = r.languages;
        const more = r.hasMoreLocal || !!r.next;
        body = (
          <>
            <RepoHits hits={r.hits} empty={<NoRepos filtered={filtered} clearHref={clear}>{emptyHint(p)}</NoRepos>} />
            <Pager
              label="Pages"
              first={p.cursor || p.page > 1 ? listHref({ cursor: "", page: 1 }, p, signedIn) : undefined}
              next={!more ? undefined : r.next ? listHref({ cursor: r.next }, p, signedIn) : listHref({ page: p.page + 1 }, p, signedIn)}
            />
            {r.truncated ? <p className="mt-2 text-sm text-ink-3">Showing the first {MEMORY_LIMIT} repositories, ordered by name.</p> : null}
          </>
        );
      } else if (p.tab === "orgs") {
        const mine = (await api.repo.listMyRepos({})).repos.map(hitOfRepo);
        languages = mine.map((h) => h.primaryLanguage);
        op = { service: "RepoService", method: "ListMyRepos", request: {}, label: "List your repositories" };
        const rows = refine(mine, p);
        total = rows.length;
        body = memoryPage(rows, p, signedIn, filtered, clear, false);
      } else {
        // Starred: the server filters by language and orders by star time or last update.
        const { items, more } = await collect<StarredRepo>(async (token) => {
          const r = await api.user.listStarred({ login: "", ...(p.language ? { language: p.language } : {}), sort: "updated", page: { pageSize: 100, ...(token ? { pageToken: token } : {}) } });
          return { items: r.repos, next: r.page?.nextPageToken ?? "" };
        }, MEMORY_LIMIT);
        op = { service: "UserService", method: "ListStarred", request: { login: "", sort: "updated", ...(p.language ? { language: p.language } : {}) }, label: "List your starred repositories" };
        languages = items.map((r) => r.language);
        const q = p.q.toLowerCase();
        const rows = items.filter((r) => !q || r.fullName.toLowerCase().includes(q) || r.description.toLowerCase().includes(q));
        if (p.sort === "name") rows.sort((a, b) => a.fullName.localeCompare(b.fullName));
        else if (p.sort === "stars") rows.sort((a, b) => Number(b.starCount) - Number(a.starCount) || a.fullName.localeCompare(b.fullName));
        total = rows.length;
        const from = (p.page - 1) * PAGE_SIZE;
        body = (
          <>
            <StarredRows repos={rows.slice(from, from + PAGE_SIZE)} empty={<NoRepos filtered={filtered} clearHref={clear}>Star a repository and it is listed here.</NoRepos>} />
            <Pager label="Pages" first={p.page > 1 ? listHref({ page: 1 }, p, signedIn) : undefined} next={from + PAGE_SIZE < rows.length ? listHref({ page: p.page + 1 }, p, signedIn) : undefined} />
            {more ? <p className="mt-2 text-sm text-ink-3">Showing the first {MEMORY_LIMIT} starred repositories.</p> : null}
          </>
        );
      }
    } catch (err) {
      if (err instanceof BeeError && err.code === "unimplemented") {
        body = <Unavailable what={p.tab === "starred" ? "Stars" : "Repository search"} message={err.message} reason={err.reason} />;
      } else {
        throw err;
      }
    }

    const tabs = REPO_TABS.filter((t) => signedIn || !t.needsSession).map((t) => ({
      href: listHref({ tab: t.id, cursor: "", page: 1 }, p, signedIn),
      label: t.label,
      active: t.id === p.tab,
    }));
    const langs = [...new Set([...COMMON_LANGUAGES, ...languages.filter(Boolean)])].sort((a, b) => a.localeCompare(b));

    return (
      <>
        <PageOp op={op} />
        <PageHeader
          title="Repositories"
          meta={total !== undefined ? <span aria-live="polite">{total === 1 ? "1 repository" : total.toLocaleString("en-US") + " repositories"}</span> : undefined}
          actions={signedIn ? <ButtonLink href="/new" variant="primary" icon="plus">New repository</ButtonLink> : undefined}
        />
        {signedIn ? <TabLinks label="Repository lists" items={tabs} /> : null}
        <div className="mb-2">
          <RepoFilters params={p} signedIn={signedIn} languages={langs} />
        </div>
        {body}
      </>
    );
  });
}

function emptyHint(p: ListParams): string {
  return p.tab === "yours" ? "Repositories owned by your account show up here. Repositories of your organizations are under Your orgs." : "No public repositories yet.";
}

/** A numbered page of an in-memory list. */
function memoryPage(rows: SearchRepoHit[], p: ListParams, signedIn: boolean, filtered: boolean, clear: string, more: boolean): React.ReactNode {
  const from = (p.page - 1) * PAGE_SIZE;
  return (
    <>
      <RepoHits hits={rows.slice(from, from + PAGE_SIZE)} empty={<NoRepos filtered={filtered} clearHref={clear}>{emptyHint(p)}</NoRepos>} />
      <Pager label="Pages" first={p.page > 1 ? listHref({ page: 1 }, p, signedIn) : undefined} next={from + PAGE_SIZE < rows.length ? listHref({ page: p.page + 1 }, p, signedIn) : undefined} />
      {more ? <p className="mt-2 text-sm text-ink-3">Showing the first {MEMORY_LIMIT} repositories, ordered by name.</p> : null}
    </>
  );
}
