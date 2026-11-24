import type { Metadata } from "next";
import Link from "next/link";
import { CopyAs, PageOp } from "@/components/copy-as";
import { Unavailable } from "@/components/error-state";
import { Badge, Button, EmptyState, Input, PageHeader } from "@/components/ui";
import { pathText } from "@/lib/api/ids";
import { serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import type { SearchCodeResponse } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";

export const metadata: Metadata = { title: "Search" };

export default async function SearchPage({ searchParams }: { searchParams: SearchParams }) {
  const sp = await searchParams;
  const q = param(sp, "q").trim();
  return guard(async () => {
    const api = await serverClient();
    const req = { query: q || "repo:" };
    let res: SearchCodeResponse | null = null;
    let unavailable: BeeError | null = null;
    // Ask even without a query: the answer says whether this server serves search at all.
    try {
      res = await api.search.searchCode(req);
    } catch (err) {
      if (err instanceof BeeError && err.code === "unimplemented") unavailable = err;
      else if (!(err instanceof BeeError && err.code === "invalid_argument" && !q)) throw err;
    }
    return (
      <>
        <PageOp op={{ service: "SearchService", method: "SearchCode", request: req, label: "Search code" }} />
        <PageHeader title="Search" meta={<span>Code across every repo you can read, with GitHub-style qualifiers (repo:, path:, lang:, symbol:).</span>} actions={<CopyAs op={{ service: "SearchService", method: "SearchCode", request: req, label: "Search code" }} />} />
        <form action="/search" className="mb-5 flex gap-2" role="search">
          <label htmlFor="q" className="sr-only">
            Search query
          </label>
          <Input id="q" name="q" defaultValue={q} placeholder="lineTotal repo:dev/playground" className="mono" disabled={!!unavailable} aria-describedby={unavailable ? "search-unavailable" : undefined} />
          <Button type="submit" variant="primary" icon="search" disabled={!!unavailable}>
            Search
          </Button>
        </form>
        {unavailable ? (
          <div id="search-unavailable">
            <Unavailable what="Code search" message={unavailable.message + ". The search index is not registered on this server, so there is nothing to show; Code and Changes still filter what they list."} reason={unavailable.reason} />
          </div>
        ) : !q ? (
          <div className="rounded-lg border border-line bg-surface">
            <EmptyState title="Type a query" icon="search" />
          </div>
        ) : (
          <div className="rounded-lg border border-line bg-surface">
            {!res || res.hits.length === 0 ? (
              <EmptyState title={"Nothing matches " + q} icon="search" />
            ) : (
              <ul className="divide-y divide-line">
                {res.hits.map((h, i) => (
                  <li key={i} className="px-3.5 py-3">
                    <p className="flex flex-wrap items-center gap-2 text-sm">
                      <span className="text-ink-3">{h.repoFullName}</span>
                      <Link href={repoHref(h.repoFullName ?? "", "code?path=" + encodeURIComponent(pathText(h.path ?? "")))} className="mono link">
                        {pathText(h.path ?? "")}
                      </Link>
                      {h.language ? <Badge>{h.language}</Badge> : null}
                    </p>
                    <pre className="mono mt-1 overflow-x-auto text-xs text-ink-2">{(h.lineMatches ?? []).map((m) => (m.lineNumber ?? 0) + ": " + (m.line ?? "")).join("\n")}</pre>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </>
    );
  });
}
