/** Reads one page of repo search results for a list view, server-ordered or (for name) ordered here. Server-only: takes the typed client. */
import type { BeeClient } from "@/lib/api/client";
import type { SearchRepoHit } from "@/lib/api/types";
import { MEMORY_LIMIT, PAGE_SIZE, refine, serverSort, type ListParams } from "./query";

/** Reads pages until `limit` items are in hand or the server has no more. */
export async function collect<T>(page: (token: string) => Promise<{ items: T[]; next: string }>, limit: number): Promise<{ items: T[]; more: boolean }> {
  const items: T[] = [];
  let token = "";
  for (;;) {
    const r = await page(token);
    items.push(...r.items);
    if (!r.next) return { items, more: false };
    if (items.length >= limit) return { items: items.slice(0, limit), more: true };
    token = r.next;
  }
}

export interface HitsPage {
  /** The hits on this page. */
  hits: SearchRepoHit[];
  total: number;
  /** Server page token of the next page; set in server-ordered views. */
  next?: string;
  /** In-memory views: whether another page of `PAGE_SIZE` follows, and whether the server held more than was read. */
  hasMoreLocal?: boolean;
  truncated?: boolean;
  /** Languages seen, to offer in the filter. */
  languages: string[];
}

/** Server-ordered for updated and stars (with a cursor); name has no server order, so up to MEMORY_LIMIT hits are read and paged here. */
export async function searchPage(api: BeeClient, query: string, p: Pick<ListParams, "sort" | "cursor" | "page">): Promise<HitsPage> {
  if (p.sort === "name") {
    const { items, more } = await collect<SearchRepoHit>(async (token) => {
      const r = await api.search.searchRepos({ query, sort: "updated", page: { pageSize: 100, ...(token ? { pageToken: token } : {}) } });
      return { items: r.hits, next: r.page?.nextPageToken ?? "" };
    }, MEMORY_LIMIT);
    const sorted = refine(items, { q: "", visibility: "", language: "", topic: "", sort: "name" });
    const from = (p.page - 1) * PAGE_SIZE;
    return { hits: sorted.slice(from, from + PAGE_SIZE), total: sorted.length, hasMoreLocal: from + PAGE_SIZE < sorted.length, truncated: more, languages: items.map((h) => h.primaryLanguage) };
  }
  const r = await api.search.searchRepos({ query, sort: serverSort(p.sort), page: { pageSize: PAGE_SIZE, ...(p.cursor ? { pageToken: p.cursor } : {}) } });
  return { hits: r.hits, total: Number(r.totalCount), next: r.page?.nextPageToken || undefined, languages: r.hits.map((h) => h.primaryLanguage) };
}
