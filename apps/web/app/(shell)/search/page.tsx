import type { Metadata } from "next";
import Link from "next/link";
import { PageFacts } from "@/components/bee/page-facts";
import { Unavailable } from "@/components/error-state";
import { MenuLink, PageMenu } from "@/components/menu";
import { Button, EmptyState, Input, PageHeader } from "@/components/ui";
import { pathText } from "@/lib/api/ids";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { SearchCodeResponse } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { toolFor } from "@/lib/tools";
import { requestMode } from "../../request-mode";

export const metadata: Metadata = { title: "Search" };

export default async function SearchPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const q = param(sp, "q").trim();
  return guard(async () => {
    const [api, { mode }, viewer] = await Promise.all([serverClient(), requestMode(), currentPrincipal()]);
    const req = { query: q || "repo:" };
    let res: SearchCodeResponse | undefined;
    let unavailable: BeeError | undefined;
    // A blank search still checks whether this server has a search index.
    try {
      res = await api.search.searchCode(req);
    } catch (err) {
      if (err instanceof BeeError && err.code === "unimplemented") unavailable = err;
      else if (!(err instanceof BeeError && err.code === "invalid_argument" && !q)) throw err;
    }
    const results = q ? (res?.hits ?? []).map((hit) => {
      const path = pathText(hit.path ?? "");
      const href = repoHref(hit.repoFullName ?? "", "code?path=" + encodeURIComponent(path));
      const lines = (hit.lineMatches ?? []).map((m) => [m.lineNumber, m.line].filter((v) => v !== undefined).join(": "));
      return { path, href, repo: hit.repoFullName, language: hit.language, lines };
    }) : [];
    const empty = q ? "No results for “" + q + "”" : "Enter a search query";
    if (mode === "bee") {
      return <PageFacts page="search" title="Search" tools={[toolFor("SearchService/SearchCode", "Search across the repositories you can read")]} groups={[
        { title: "query", facts: q ? [{ label: "query", value: q }] : [], empty: "Use repo:, path:, lang: or symbol: to narrow your search." },
        { title: "results", lines: results.map((r) => [r.repo, r.path, r.language, r.href, ...r.lines].filter(Boolean).join(" · ")), empty: unavailable ? unavailable.reason + " · " + unavailable.message : empty },
      ]} />;
    }
    return (
      <>
        <PageHeader title="Search" actions={<PageMenu op={{ service: "SearchService", method: "SearchCode", request: req, label: "Search code" }}>
          {q ? <MenuLink href="/search">Clear query</MenuLink> : null}
        </PageMenu>} />
        <form action="/search" className="mb-8 max-w-2xl" role="search" aria-label="Search code">
          <label htmlFor="q" className="sr-only">Search query</label>
          <div className="flex gap-2">
            <Input id="q" name="q" defaultValue={q} placeholder="Search code" disabled={!!unavailable} aria-describedby={unavailable ? "search-unavailable" : "search-hint"} />
            <Button type="submit" variant={viewer ? "primary" : "secondary"} disabled={!!unavailable}>Search</Button>
          </div>
          <p id="search-hint" className="mt-2 text-sm text-ink-2">
            Narrow your search with <code>repo:</code>, <code>path:</code>, <code>lang:</code>, or <code>symbol:</code>.
          </p>
        </form>
        {unavailable ? (
          <div id="search-unavailable">
            <Unavailable what="Code search" message={unavailable.message} reason={unavailable.reason} />
          </div>
        ) : results.length === 0 ? (
          <EmptyState title={empty} />
        ) : (
          <ul className="rule divide-y divide-line" aria-label="Code search results">
            {results.map((r, i) => (
              <li key={r.href + ":" + i} className="min-w-0 py-4">
                <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                  <span className="text-ink-2 [overflow-wrap:anywhere]">{r.repo}</span>
                  <Link href={r.href} className="mono link inline-flex min-h-6 min-w-0 items-center break-all" data-nav-item>{r.path}</Link>
                  {r.language ? <span className="text-ink-3">{r.language}</span> : null}
                </p>
                {r.lines.length ? <pre className="mono mt-1 overflow-x-auto text-sm text-ink-2">{r.lines.join("\n")}</pre> : null}
              </li>
            ))}
          </ul>
        )}
      </>
    );
  });
}
