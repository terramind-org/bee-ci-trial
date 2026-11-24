/**
 * SearchService for the fixture: repo and user search with the query qualifiers of
 * docs/spec/search.md, trending, leaderboards and topics. The candidate set is always what the
 * caller may read (public repos for an anonymous caller), so nothing private leaks into a count.
 */
import { SearchService, type CodeSearchHitJson, type SearchRepoHitJson, type TopicJson } from "../../gen/bee/v1/search_pb";
import { pathBytes } from "../ids";
import { objectId } from "./seed";
import { TOPICS, type TopicSeed } from "./community-world";
import { BeeError } from "../transport";
import { defineService } from "./router";
import type { FixtureStore, RepoState } from "./store";
import { invalid, page } from "./util";
import {
  accounts,
  followerCount,
  landsByAuthor,
  landsIn,
  profileFacts,
  publicRepoCount,
  repoHit,
  reviewsByReviewer,
  starsIn,
  userHit,
  windowMs,
  displayName,
  type Account,
} from "./social";

// ------------------------------------------------------------------ query parsing

interface Qualifier {
  key: string;
  value: string;
  negated: boolean;
}

interface Parsed {
  terms: string[];
  quals: Qualifier[];
}

const unquote = (v: string) => v.replace(/^"(.*)"$/, "$1");

/** Splits a query into free-text terms and `key:value` qualifiers whose key is in `known`. */
export function parseQuery(query: string | undefined, known: ReadonlySet<string>): Parsed {
  const terms: string[] = [];
  const quals: Qualifier[] = [];
  for (const m of (query ?? "").matchAll(/(-?)([A-Za-z][A-Za-z-]*):("[^"]*"|\S+)|"([^"]*)"|\S+/g)) {
    if (m[2] && known.has(m[2].toLowerCase())) quals.push({ key: m[2].toLowerCase(), value: unquote(m[3] ?? ""), negated: m[1] === "-" });
    else terms.push((m[4] ?? m[0]).toLowerCase());
  }
  return { terms, quals };
}

type Range = { min: number; max: number };

/** `>5`, `>=5`, `<5`, `<=5`, `5`, `1..9`, `*..9`, `1..*`. */
function numberRange(v: string): Range {
  const bad = () => invalid("cannot read the number or range " + v, "INVALID_QUERY");
  const num = (x: string) => {
    if (!/^\d+$/.test(x)) throw bad();
    return Number(x);
  };
  let m = /^(\d+|\*)\.\.(\d+|\*)$/.exec(v);
  if (m) return { min: m[1] === "*" ? -Infinity : num(m[1] ?? ""), max: m[2] === "*" ? Infinity : num(m[2] ?? "") };
  m = /^(>=|<=|>|<)(\d+)$/.exec(v);
  if (m) {
    const n = num(m[2] ?? "");
    return m[1] === ">" ? { min: n + 1, max: Infinity } : m[1] === ">=" ? { min: n, max: Infinity } : m[1] === "<" ? { min: -Infinity, max: n - 1 } : { min: -Infinity, max: n };
  }
  return { min: num(v), max: num(v) };
}

const inRange = (n: number, r: Range) => n >= r.min && n <= r.max;
const topicName = (t: string) => resolveTopic(t)?.name ?? t.toLowerCase();

function order(req: { order?: string }, quals: Qualifier[]): 1 | -1 {
  const q = quals.find((x) => x.key === "order")?.value.toLowerCase();
  return q === "asc" || (!q && req.order === "SEARCH_SORT_ORDER_ASC") ? -1 : 1;
}

// ------------------------------------------------------------------ repo search

const REPO_KEYS = new Set(["is", "topic", "language", "lang", "org", "user", "repo", "license", "stars", "forks", "archived", "in", "sort", "order"]);

function repoTextScore(r: RepoState, terms: string[], fields: ReadonlySet<string>): number {
  let score = 0;
  for (const t of terms) {
    const name = r.repo.name.toLowerCase();
    let s = 0;
    if (fields.has("name")) s = Math.max(s, name === t ? 10 : name.includes(t) ? 5 : r.repo.fullName.toLowerCase().includes(t) ? 3 : 0);
    if (fields.has("topics") && r.topics.some((x) => x.toLowerCase() === t)) s = Math.max(s, 4);
    if (fields.has("description") && r.repo.description.toLowerCase().includes(t)) s = Math.max(s, 2);
    if (s === 0) return -1;
    score += s;
  }
  return score;
}

function matchRepo(s: FixtureStore, r: RepoState, q: Qualifier): boolean {
  const v = q.value.toLowerCase();
  const view = s.repoView(r);
  switch (q.key) {
    case "is":
      switch (v) {
        case "public":
        case "private":
        case "internal":
          return r.repo.visibility === "VISIBILITY_" + v.toUpperCase();
        case "archived":
          return r.archived;
        case "fork":
        case "template":
        case "mirror":
        case "sponsorable":
          return false;
        default:
          throw invalid("is:" + q.value + " is not a repo filter (use public, private, internal, archived, fork, template, mirror or sponsorable)", "INVALID_QUERY");
      }
    case "topic":
      return r.topics.some((t) => topicName(t) === topicName(v));
    case "language":
    case "lang":
      return view.primaryLanguage.toLowerCase() === v;
    case "org":
      return r.owner.toLowerCase() === v;
    case "user":
      // People own nothing in this world; `user:x` finds the repos x has landed work in.
      return r.owner.toLowerCase() === v || r.main.some((c) => s.loginByHandle.get(c.author)?.toLowerCase() === v);
    case "repo":
      return r.repo.fullName.toLowerCase() === v;
    case "license":
      return view.licenseSpdx.toLowerCase() === v;
    case "stars":
      return inRange(Number(view.stars), numberRange(v));
    case "forks":
      return inRange(Number(view.forks), numberRange(v));
    case "archived":
      return r.archived === (v === "true");
    default:
      return true;
  }
}

const REPO_SORTS = new Set(["", "stars", "forks", "updated", "best-match"]);

function searchRepos(s: FixtureStore, viewer: string | undefined, req: { query?: string; sort?: string; order?: string; page?: { pageSize?: number; pageToken?: string } }) {
  const { terms, quals } = parseQuery(req.query, REPO_KEYS);
  const inFields = quals.find((x) => x.key === "in");
  const fields = new Set(inFields ? inFields.value.toLowerCase().split(",") : ["name", "description", "topics"]);
  const sort = (quals.find((x) => x.key === "sort")?.value ?? req.sort ?? "").toLowerCase();
  if (!REPO_SORTS.has(sort)) throw invalid("cannot sort repos by " + sort + " (use stars, forks, updated or best-match)", "INVALID_SORT");
  const dir = order(req, quals);
  const filters = quals.filter((x) => !["in", "sort", "order"].includes(x.key));
  const rows: { r: RepoState; score: number }[] = [];
  for (const r of s.readableRepos(viewer)) {
    if (!filters.every((q) => matchRepo(s, r, q) !== q.negated)) continue;
    const score = terms.length ? repoTextScore(r, terms, fields) : 0;
    if (score < 0) continue;
    rows.push({ r, score });
  }
  const stars = (r: RepoState) => s.stars.get(r.name)?.length ?? 0;
  const updated = (r: RepoState) => r.refs.get(r.repo.defaultRef)?.target.time ?? 0;
  const key = (x: { r: RepoState; score: number }) => (sort === "stars" ? stars(x.r) : sort === "forks" ? (s.forks.get(x.r.name)?.length ?? 0) : sort === "updated" ? updated(x.r) : x.score);
  rows.sort((a, b) => dir * (key(b) - key(a)) || stars(b.r) - stars(a.r) || a.r.repo.fullName.localeCompare(b.r.repo.fullName));
  const pg = page(rows, req.page, 30);
  return { hits: pg.items.map((x) => repoHit(s, x.r, x.score)), totalCount: String(rows.length), incomplete: false, page: { nextPageToken: pg.next } };
}

// ------------------------------------------------------------------ user search

const USER_KEYS = new Set(["type", "in", "repos", "followers", "location", "sort", "order"]);
const USER_SORTS = new Set(["", "followers", "repositories", "joined", "best-match"]);

function searchUsers(s: FixtureStore, req: { query?: string; sort?: string; order?: string; page?: { pageSize?: number; pageToken?: string } }) {
  const { terms, quals } = parseQuery(req.query, USER_KEYS);
  const type = quals.find((x) => x.key === "type")?.value.toLowerCase();
  if (type && !["user", "org", "agent"].includes(type)) throw invalid("type:" + type + " is not a user type (use user, org or agent)", "INVALID_QUERY");
  const inFields = new Set((quals.find((x) => x.key === "in")?.value ?? "login,name,bio").toLowerCase().split(","));
  const sort = (quals.find((x) => x.key === "sort")?.value ?? req.sort ?? "").toLowerCase();
  if (!USER_SORTS.has(sort)) throw invalid("cannot sort users by " + sort + " (use followers, repositories, joined or best-match)", "INVALID_SORT");
  const dir = order(req, quals);
  const followers = quals.filter((x) => x.key === "followers").map((x) => numberRange(x.value));
  const repos = quals.filter((x) => x.key === "repos").map((x) => numberRange(x.value));
  const location = quals.filter((x) => x.key === "location").map((x) => x.value.toLowerCase());
  const rows: { a: Account; score: number }[] = [];
  for (const a of accounts(s)) {
    if (type && a.kind !== type) continue;
    if (!followers.every((r) => inRange(followerCount(s, a.login), r))) continue;
    if (!repos.every((r) => inRange(publicRepoCount(s, a), r))) continue;
    const facts = profileFacts(s, a);
    if (!location.every((l) => facts.location.toLowerCase().includes(l))) continue;
    let score = 0;
    let ok = true;
    for (const t of terms) {
      const login = a.login.toLowerCase();
      let m = 0;
      if (inFields.has("login")) m = Math.max(m, login === t ? 10 : login.startsWith(t) ? 6 : login.includes(t) ? 4 : 0);
      if (inFields.has("name") && displayName(s, a).toLowerCase().includes(t)) m = Math.max(m, 3);
      if (inFields.has("bio") && facts.bio.toLowerCase().includes(t)) m = Math.max(m, 1);
      if (m === 0) ok = false;
      score += m;
    }
    if (ok) rows.push({ a, score });
  }
  const key = (x: { a: Account; score: number }) =>
    sort === "followers" ? followerCount(s, x.a.login) : sort === "repositories" ? publicRepoCount(s, x.a) : sort === "joined" ? profileFacts(s, x.a).createdAt : x.score;
  rows.sort((a, b) => dir * (key(b) - key(a)) || followerCount(s, b.a.login) - followerCount(s, a.a.login) || a.a.login.localeCompare(b.a.login));
  const pg = page(rows, req.page, 30);
  return { hits: pg.items.map((x) => userHit(s, x.a)), totalCount: String(rows.length), incomplete: false, page: { nextPageToken: pg.next } };
}

// ------------------------------------------------------------------ topics

export function resolveTopic(name: string | undefined): TopicSeed | undefined {
  const n = (name ?? "").trim().toLowerCase();
  return TOPICS.find((t) => t.name === n || t.aliases.includes(n));
}

function topicJson(s: FixtureStore, viewer: string | undefined, seed: Pick<TopicSeed, "name"> & Partial<TopicSeed>, topCount: number): TopicJson {
  const repos = s.readableRepos(viewer).filter((r) => r.topics.some((t) => topicName(t) === seed.name));
  const stars = (r: RepoState) => s.stars.get(r.name)?.length ?? 0;
  const top = [...repos].sort((a, b) => stars(b) - stars(a) || a.repo.fullName.localeCompare(b.repo.fullName)).slice(0, topCount);
  const curated = !!TOPICS.find((t) => t.name === seed.name);
  return {
    name: seed.name,
    displayName: seed.displayName ?? seed.name,
    shortDescription: seed.shortDescription ?? "",
    description: seed.description ?? "",
    featured: seed.featured ?? false,
    curated,
    aliases: seed.aliases ?? [],
    related: seed.related ?? [],
    logoUrl: "",
    released: seed.released ?? "",
    createdBy: seed.createdBy ?? "",
    wikipediaUrl: "",
    repoCount: String(repos.length),
    topRepos: top.map((r) => repoHit(s, r)),
  };
}

// ------------------------------------------------------------------ the service


// ------------------------------------------------------------------ code search

const CODE_QUALIFIERS: ReadonlySet<string> = new Set(["repo", "path", "lang"]);
const LANGUAGE_OF: Record<string, string> = { ts: "TypeScript", tsx: "TypeScript", js: "JavaScript", json: "JSON", md: "Markdown", yaml: "YAML", yml: "YAML", tf: "HCL", rs: "Rust", py: "Python", sql: "SQL" };

const languageOfPath = (path: string) => LANGUAGE_OF[path.split(".").pop()?.toLowerCase() ?? ""] ?? "";

/** Matching lines of the tip of every readable repo's main, in path order; a query without terms matches nothing. */
function searchCode(s: FixtureStore, viewer: string | undefined, req: { query?: string; caseSensitive?: boolean; maxLineMatchesPerFile?: number; page?: { pageSize?: number; pageToken?: string } }) {
  const { terms, quals } = parseQuery(req.query, CODE_QUALIFIERS);
  const fold = (v: string) => (req.caseSensitive ? v : v.toLowerCase());
  const needles = terms.map(fold).filter(Boolean);
  const qual = (key: string) => quals.filter((q) => q.key === key);
  const keep = (key: string, value: string, test: (have: string, want: string) => boolean) => qual(key).every((q) => test(value.toLowerCase(), q.value.toLowerCase()) !== q.negated);
  const perFile = Math.max(1, req.maxLineMatchesPerFile || 5);
  const hits: CodeSearchHitJson[] = [];
  if (needles.length === 0) return { hits, totalCount: "0", incomplete: false, facets: [], page: { nextPageToken: "" } };
  for (const r of s.readableRepos(viewer).sort((a, b) => a.repo.fullName.localeCompare(b.repo.fullName))) {
    if (!keep("repo", r.repo.fullName, (have, want) => have === want || have.endsWith("/" + want))) continue;
    const tip = r.main[r.main.length - 1];
    if (!tip) continue;
    for (const path of [...tip.files.keys()].sort()) {
      const content = tip.files.get(path) ?? "";
      const language = languageOfPath(path);
      if (!keep("path", path, (have, want) => have.includes(want)) || !keep("lang", language, (have, want) => have === want)) continue;
      const lines = content.split("\n");
      const matches = lines.flatMap((line, i) => (needles.every((n) => fold(line).includes(n)) ? [{ lineNumber: i + 1, line, contextBefore: lines.slice(Math.max(0, i - 1), i), contextAfter: lines.slice(i + 1, i + 2) }] : []));
      if (matches.length === 0) continue;
      hits.push({
        repo: { fullName: r.repo.fullName },
        repoFullName: r.repo.fullName,
        path: pathBytes(path),
        ref: "main",
        changeset: tip.id,
        blob: objectId("blob", content),
        language,
        lineMatches: matches.slice(0, perFile),
        matchCount: matches.length,
        score: matches.length,
      });
    }
  }
  const pg = page(hits, req.page, 50);
  return { hits: pg.items, totalCount: String(hits.length), incomplete: false, facets: [], page: { nextPageToken: pg.next } };
}

export const discovery = defineService(SearchService, {
  searchCode: (s, req, ctx) => searchCode(s, s.viewer(ctx.token), req),
  searchRepos: (s, req, ctx) => searchRepos(s, s.viewer(ctx.token), req),
  searchUsers: (s, req) => searchUsers(s, req),

  listTrendingRepos: (s, req, ctx) => {
    const since = s.anchor - windowMs(req.window);
    const lang = (req.language ?? "").toLowerCase();
    const rows = s
      .readableRepos(s.viewer(ctx.token))
      .filter((r) => !lang || s.repoView(r).primaryLanguage.toLowerCase() === lang)
      .map((r) => ({ r, stars: starsIn(s, r, since), lands: landsIn(s, r, since) }))
      .filter((x) => x.stars > 0 || x.lands > 0)
      .sort((a, b) => b.stars - a.stars || b.lands - a.lands || a.r.repo.fullName.localeCompare(b.r.repo.fullName));
    const pg = page(rows, req.page, 25);
    const start = pg.items.length ? rows.indexOf(pg.items[0] as (typeof rows)[number]) : 0;
    return {
      repos: pg.items.map((x, i) => {
        const builders = [...landsByAuthor(s, x.r, since).entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).slice(0, 3);
        return {
          repo: repoHit(s, x.r),
          starsInWindow: String(x.stars),
          forksInWindow: String((s.forks.get(x.r.name) ?? []).filter((t) => t > since).length),
          builtBy: builders.map(([h]) => s.p(h)),
          rank: start + i + 1,
        };
      }),
      computedAt: s.iso(s.anchor),
      page: { nextPageToken: pg.next },
    };
  },

  listTrendingDevelopers: (s, req, ctx) => {
    const since = s.anchor - windowMs(req.window);
    const lang = (req.language ?? "").toLowerCase();
    const repos = s.readableRepos(s.viewer(ctx.token)).filter((r) => !lang || s.repoView(r).primaryLanguage.toLowerCase() === lang);
    const totals = new Map<string, { lands: number; byRepo: Map<RepoState, number> }>();
    for (const r of repos)
      for (const [h, n] of landsByAuthor(s, r, since)) {
        if (s.kindOf(h) === "agent" && !req.includeAgents) continue;
        const row = totals.get(h) ?? { lands: 0, byRepo: new Map<RepoState, number>() };
        row.lands += n;
        row.byRepo.set(r, n);
        totals.set(h, row);
      }
    const rows = [...totals.entries()]
      .map(([h, t]) => ({ h, t, login: s.loginByHandle.get(h) ?? h }))
      .sort((a, b) => b.t.lands - a.t.lands || a.login.localeCompare(b.login));
    const pg = page(rows, req.page, 25);
    const start = pg.items.length ? rows.indexOf(pg.items[0] as (typeof rows)[number]) : 0;
    const byLogin = new Map(accounts(s).map((a) => [a.handle, a]));
    return {
      developers: pg.items.map((x, i) => {
        const popular = [...x.t.byRepo.entries()].sort((a, b) => b[1] - a[1] || a[0].repo.fullName.localeCompare(b[0].repo.fullName))[0]?.[0];
        return { user: userHit(s, byLogin.get(x.h) as Account), popularRepo: popular ? repoHit(s, popular) : undefined, rank: start + i + 1 };
      }),
      computedAt: s.iso(s.anchor),
      page: { nextPageToken: pg.next },
    };
  },

  listLeaderboard: (s, req, ctx) => {
    const subject = req.subject ?? "LEADERBOARD_SUBJECT_UNSPECIFIED";
    const metric = req.metric ?? "LEADERBOARD_METRIC_UNSPECIFIED";
    if (subject === "LEADERBOARD_SUBJECT_UNSPECIFIED") throw invalid("pick a subject: AGENTS, HUMANS or REPOS", "INVALID_FIELD");
    if (metric === "LEADERBOARD_METRIC_UNSPECIFIED") throw invalid("pick a metric: LANDS, REVIEWS or STARS", "INVALID_FIELD");
    if (subject === "LEADERBOARD_SUBJECT_REPOS" && metric === "LEADERBOARD_METRIC_REVIEWS") throw invalid("metric REVIEWS ranks agents or humans, not repos", "INVALID_FIELD");
    if (subject !== "LEADERBOARD_SUBJECT_REPOS" && metric === "LEADERBOARD_METRIC_STARS") throw invalid("metric STARS ranks repos, not agents or humans", "INVALID_FIELD");
    const since = s.anchor - windowMs(req.window);
    const repos = s.readableRepos(s.viewer(ctx.token));
    type Row = { score: number; name: string; entry: { user?: ReturnType<typeof userHit>; repo?: SearchRepoHitJson } };
    const rows: Row[] = [];
    if (subject === "LEADERBOARD_SUBJECT_REPOS") {
      for (const r of repos) rows.push({ score: metric === "LEADERBOARD_METRIC_STARS" ? starsIn(s, r, since) : landsIn(s, r, since), name: r.repo.fullName, entry: { repo: repoHit(s, r) } });
    } else {
      const counts = new Map<string, number>();
      if (metric === "LEADERBOARD_METRIC_REVIEWS") for (const [h, n] of reviewsByReviewer(s, repos, since)) counts.set(h, n);
      else for (const r of repos) for (const [h, n] of landsByAuthor(s, r, since)) counts.set(h, (counts.get(h) ?? 0) + n);
      const want = subject === "LEADERBOARD_SUBJECT_AGENTS" ? "agent" : "user";
      for (const a of accounts(s)) if (a.kind === want && a.handle) rows.push({ score: counts.get(a.handle) ?? 0, name: a.login, entry: { user: userHit(s, a) } });
    }
    const ranked = rows.filter((x) => x.score > 0).sort((a, b) => b.score - a.score || a.name.localeCompare(b.name));
    const pg = page(ranked, req.page, 25);
    const start = pg.items.length ? ranked.indexOf(pg.items[0] as Row) : 0;
    return { entries: pg.items.map((x, i) => ({ rank: start + i + 1, score: String(x.score), ...x.entry })), computedAt: s.iso(s.anchor), page: { nextPageToken: pg.next } };
  },

  getTopic: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const seed = resolveTopic(req.name);
    const name = (req.name ?? "").trim().toLowerCase();
    const top = req.topRepoCount || 5;
    if (seed) return topicJson(s, viewer, seed, top);
    // Not curated: a topic exists when a repo the caller can read uses it.
    const used = s.readableRepos(viewer).some((r) => r.topics.includes(name));
    if (!name || !used) throw new BeeError("not_found", { reason: "TOPIC_NOT_FOUND", message: "no topic named " + (name || "(empty)") });
    return topicJson(s, viewer, { name }, top);
  },

  listFeaturedTopics: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    const featured = TOPICS.filter((t) => t.featured).sort((a, b) => a.name.localeCompare(b.name));
    const pg = page(featured, req.page, 25);
    return { topics: pg.items.map((t) => topicJson(s, viewer, t, 3)), page: { nextPageToken: pg.next } };
  },
});
