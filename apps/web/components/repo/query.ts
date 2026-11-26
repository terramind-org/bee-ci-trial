/**
 * The /repositories query string and the search query it becomes. Pure, so the page, the filter
 * bar and the tests agree on one reading of the URL.
 */
import type { Repo, SearchRepoHit, Visibility } from "@/lib/api/types";

export type RepoTab = "yours" | "orgs" | "starred" | "public";
export type RepoSort = "updated" | "stars" | "name";
export type VisibilityFilter = "" | "public" | "private" | "internal";

export const REPO_TABS: { id: RepoTab; label: string; needsSession: boolean }[] = [
  { id: "yours", label: "Yours", needsSession: true },
  { id: "orgs", label: "Your orgs", needsSession: true },
  { id: "starred", label: "Starred", needsSession: true },
  { id: "public", label: "Public", needsSession: false },
];

export const PAGE_SIZE = 20;
/** How many repos the in-memory views (name sort, your orgs, starred) read before paging locally. */
export const MEMORY_LIMIT = 200;

export interface ListParams {
  tab: RepoTab;
  q: string;
  visibility: VisibilityFilter;
  language: string;
  topic: string;
  sort: RepoSort;
  /** A server page token (`cursor`), or "" for the first page. */
  cursor: string;
  /** A 1-based page number for the views that page in memory. */
  page: number;
}

type Raw = Record<string, string | string[] | undefined>;
const one = (v: string | string[] | undefined) => (Array.isArray(v) ? (v[0] ?? "") : (v ?? ""));
const clean = (s: string) => s.trim().replace(/\s+/g, " ");
/** A qualifier value is one word; anything else would let a filter smuggle in more qualifiers. */
const word = (s: string) => clean(s).replace(/[\s:"]+/g, "");

/** Reads the URL. Signed-out visitors only ever see the public tab. */
export function parseListParams(sp: Raw, signedIn: boolean): ListParams {
  const asked = one(sp.tab);
  const tab: RepoTab = signedIn ? (REPO_TABS.some((t) => t.id === asked) ? (asked as RepoTab) : "yours") : "public";
  const vis = one(sp.visibility);
  const sort = one(sp.sort);
  const page = Math.floor(Number(one(sp.page)));
  return {
    tab,
    q: clean(one(sp.q)),
    visibility: vis === "public" || vis === "private" || vis === "internal" ? vis : "",
    language: word(one(sp.language)),
    topic: word(one(sp.topic)).toLowerCase(),
    sort: sort === "stars" || sort === "name" ? sort : "updated",
    cursor: one(sp.cursor),
    page: Number.isFinite(page) && page > 1 ? page : 1,
  };
}

/** Where a filter bar writes: `/repositories`, or a fixed page of a profile (`/acme?tab=repositories`). */
export interface ListTarget {
  path: string;
  /** Query parameters the page needs to stay on this view. */
  fixed?: Record<string, string>;
}

/** The URL for these params, leaving defaults out so a bare `/repositories` stays bare. */
export function listHref(p: Partial<ListParams>, base: ListParams, signedIn: boolean, target?: ListTarget): string {
  const m = { ...base, ...p };
  const qs = new URLSearchParams(target?.fixed);
  if (!target && signedIn && m.tab !== "yours") qs.set("tab", m.tab);
  if (m.q) qs.set("q", m.q);
  if (m.visibility) qs.set("visibility", m.visibility);
  if (m.language) qs.set("language", m.language);
  if (m.topic) qs.set("topic", m.topic);
  if (m.sort !== "updated") qs.set("sort", m.sort);
  if (m.cursor) qs.set("cursor", m.cursor);
  if (m.page > 1) qs.set("page", String(m.page));
  const s = qs.toString();
  return (target?.path ?? "/repositories") + (s ? "?" + s : "");
}

/** The search query for a profile's Repositories view: the owner's repos plus the filters. */
export function ownerQuery(p: Pick<ListParams, "q" | "visibility" | "language" | "topic">, kind: "org" | "user", name: string): string {
  const parts = [kind + ":" + word(name)];
  if (p.visibility) parts.push("is:" + p.visibility);
  if (p.language) parts.push("language:" + p.language);
  if (p.topic) parts.push("topic:" + p.topic);
  if (p.q) parts.push(p.q);
  return parts.join(" ");
}

/** The search query for the tabs the server filters: text plus `is:`, `language:`, `topic:`, `user:`. */
export function searchQuery(p: ListParams, login: string): string {
  const parts: string[] = [];
  if (p.tab === "yours" && login) parts.push("user:" + word(login));
  if (p.tab === "public") parts.push("is:public");
  else if (p.visibility) parts.push("is:" + p.visibility);
  if (p.language) parts.push("language:" + p.language);
  if (p.topic) parts.push("topic:" + p.topic);
  if (p.q) parts.push(p.q);
  return parts.join(" ");
}

/** The `sort` argument of SearchRepos; name has no server order, so it is sorted in memory. */
export function serverSort(sort: RepoSort): string {
  return sort === "stars" ? "stars" : "updated";
}

/** "public", "internal" or "private": the word search hits carry (`VISIBILITY_PUBLIC` from RepoService is mapped). */
export function visibilityWord(v: string): "public" | "internal" | "private" {
  const w = v.replace(/^VISIBILITY_/, "").toLowerCase();
  return w === "public" || w === "internal" ? w : "private";
}

/** The enum RepoCard prints, from either spelling. */
export function visibilityOf(v: string): Visibility {
  return ("VISIBILITY_" + visibilityWord(v).toUpperCase()) as Visibility;
}

/** A repo from RepoService, shaped like a search hit so one card renders both. */
export function hitOfRepo(r: Repo): SearchRepoHit {
  return {
    fullName: r.fullName,
    description: r.description,
    topics: r.topics,
    primaryLanguage: r.primaryLanguage,
    starCount: r.stars,
    forkCount: r.forks,
    openIssueCount: r.openIssues,
    licenseSpdxId: r.licenseSpdx,
    archived: r.archived,
    isFork: false,
    isTemplate: false,
    visibility: visibilityWord(r.visibility),
    updatedAt: r.pushedAt,
    pushedAt: r.pushedAt,
  };
}

const time = (iso: string | undefined) => Date.parse(iso ?? "") || 0;

/** Filters and orders repos the page holds in memory the way the server would. */
export function refine(hits: SearchRepoHit[], p: Pick<ListParams, "q" | "visibility" | "language" | "topic" | "sort">): SearchRepoHit[] {
  const q = p.q.toLowerCase();
  const out = hits.filter(
    (h) =>
      (!p.visibility || visibilityWord(h.visibility) === p.visibility) &&
      (!p.language || h.primaryLanguage.toLowerCase() === p.language.toLowerCase()) &&
      (!p.topic || h.topics.some((t) => t.toLowerCase() === p.topic)) &&
      (!q || h.fullName.toLowerCase().includes(q) || h.description.toLowerCase().includes(q) || h.topics.some((t) => t.toLowerCase().includes(q))),
  );
  const stars = (h: SearchRepoHit) => Number(h.starCount);
  out.sort((a, b) =>
    p.sort === "name"
      ? a.fullName.localeCompare(b.fullName)
      : p.sort === "stars"
        ? stars(b) - stars(a) || a.fullName.localeCompare(b.fullName)
        : time(b.pushedAt ?? b.updatedAt) - time(a.pushedAt ?? a.updatedAt) || a.fullName.localeCompare(b.fullName),
  );
  return out;
}

/** Languages the filter always offers, joined by whatever the visible repos use. */
export const COMMON_LANGUAGES = ["C", "C++", "C#", "CSS", "Go", "HTML", "Java", "JavaScript", "Kotlin", "Python", "Ruby", "Rust", "Shell", "Swift", "TypeScript"];
