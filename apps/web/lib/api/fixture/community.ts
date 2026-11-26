/**
 * Fixture handlers for community and discovery: users (profiles, stars, follows, the
 * contribution graph, achievements, the home feed), search, insights, and org and repo
 * creation. The anonymous gate in `anonymous.ts` has already run when these do, so a handler
 * only decides what the caller sees of the data it is allowed to ask about.
 */
import { InsightsService } from "../../gen/bee/v1/insights_pb";
import type { OrgService, RepoService } from "../../gen/bee/v1/forge_pb";
import { UserService, type UserAchievementJson, type UserFeedEventJson } from "../../gen/bee/v1/users_pb";
import { BeeError } from "../transport";
import { discovery } from "./discovery";
import { repoFollowImpl } from "./posts";
import { defineService, type Ctx, type Impl, type ServiceDef } from "./router";
import { fnv1a, prng, ulid } from "./seed";
import type { FixtureStore, RepoState } from "./store";
import { invalid, page } from "./util";
import {
  DAY,
  accountByLogin,
  displayName,
  followerCount,
  followingCount,
  isFollowing,
  isService,
  loginOf,
  principalOf,
  profileFacts,
  publicRepoCount,
  starCountOf,
  summary,
  userAccount,
  userNotFound,
  type Account,
} from "./social";

const RESERVED_ORGS = new Set(["api", "admin", "explore", "trending", "topics", "leaderboards", "repositories", "pricing", "new", "prompts", "signin", "orgs", "import", "search", "settings"]);
const ORG_NAME = /^[a-z0-9][a-z0-9-]{0,38}$/;
const REPO_NAME = /^[a-z0-9][a-z0-9._-]{0,99}$/;
const TOPIC_NAME = /^[a-z0-9][a-z0-9-]{0,49}$/;
const LICENSES: Record<string, string> = { mit: "MIT", "apache-2.0": "Apache-2.0", "gpl-3.0": "GPL-3.0", "bsd-3-clause": "BSD-3-Clause", "mpl-2.0": "MPL-2.0", unlicense: "Unlicense" };

/** The signed-in caller of a request the gate has already authenticated. */
const me = (s: FixtureStore, ctx: Ctx): string => s.caller(ctx.token);

const lower = (x: string) => x.toLowerCase();

// ------------------------------------------------------------------ org and repo creation

export const createOrg: NonNullable<Impl<typeof OrgService>["createOrg"]> = (s, req, ctx) => {
  const actor = me(s, ctx);
  const name = req.name ?? "";
  if (!ORG_NAME.test(name)) throw invalid("org names are 1 to 39 lowercase letters, digits or hyphens, and start with a letter or digit", "INVALID_NAME");
  if (RESERVED_ORGS.has(name)) throw invalid(name + " is reserved", "RESERVED_NAME");
  if (s.orgs.has(name) || s.handleByLogin.has(name)) throw new BeeError("already_exists", { reason: "ORG_EXISTS", message: "the name " + name + " is taken" });
  const at = s.now();
  const org = { id: ulid(at, "org:" + name), name, display: req.display?.trim() || name, createdAt: s.iso(at) };
  s.orgs.set(name, { org, description: "", website: "", location: "", email: "", members: new Map([[actor, "ROLE_OWNER" as const]]) });
  return org;
};

export const createRepo: NonNullable<Impl<typeof RepoService>["createRepo"]> = (s, req, ctx) => {
  const actor = me(s, ctx);
  const owner = s.orgRecord(req.org);
  if (!s.inOrg(actor, owner.org.name)) throw new BeeError("permission_denied", { reason: "NOT_A_MEMBER", message: "you are not a member of " + owner.org.name });
  const name = req.name ?? "";
  if (!REPO_NAME.test(name)) throw invalid("repo names are 1 to 100 lowercase letters, digits, dots, underscores or hyphens, and start with a letter or digit", "INVALID_NAME");
  if (s.repos.has(owner.org.name === s.org.name ? name : owner.org.name + "/" + name)) throw new BeeError("already_exists", { reason: "REPO_EXISTS", message: owner.org.name + "/" + name + " already exists" });
  const topics = [...new Set((req.topics ?? []).map(lower))];
  const badTopic = topics.find((t) => !TOPIC_NAME.test(t));
  if (badTopic) throw invalid("topic " + badTopic + " must be lowercase letters, digits or hyphens", "INVALID_TOPIC");
  const visibility = !req.visibility || req.visibility === "VISIBILITY_UNSPECIFIED" ? "VISIBILITY_PRIVATE" : req.visibility;
  const license = req.licenseTemplate ? (LICENSES[lower(req.licenseTemplate)] ?? req.licenseTemplate) : "";
  const at = s.now();
  const repo = s.addRepo(owner, name, { visibility, description: req.description ?? "", topics, license, homepage: req.homepage ?? "", createdAt: at, idAt: at });
  // A repo's creator follows it automatically.
  s.posts.follow(s, actor, repo.name);
  if (req.autoInit) {
    const readme = "# " + name + "\n\n" + (req.description ? req.description + "\n" : "");
    const commit = s.makeCommit(repo.name, null, { "README.md": readme }, actor, at, "Initial commit", repo.name + "@0");
    s.advanceMain(repo, commit, actor);
  }
  return s.repoView(repo);
};

// ------------------------------------------------------------------ users

function profileJson(s: FixtureStore, viewer: string | undefined, a: Account) {
  const f = profileFacts(s, a);
  const handle = a.handle ?? "";
  const viewerLogin = viewer ? loginOf(s, viewer) : undefined;
  return {
    principal: principalOf(s, a),
    login: a.login,
    name: displayName(s, a),
    bio: f.bio,
    pronouns: f.pronouns ?? "",
    company: f.company,
    location: f.location,
    website: f.website,
    avatarUrl: "",
    readmeHtml: f.readmeHtml ?? "",
    hireable: false,
    orgs: [...s.orgs.keys()].filter((o) => s.inOrg(handle, o)),
    followerCount: String(followerCount(s, a.login)),
    followingCount: String(followingCount(s, a.login)),
    publicRepoCount: String(publicRepoCount(s, a)),
    publicGistCount: "0",
    starCount: String(starCountOf(s, a.login)),
    viewerIsFollowing: isFollowing(s, viewerLogin, a.login),
    followsViewer: !!viewerLogin && isFollowing(s, a.login, viewerLogin),
    isSiteAdmin: false,
    sponsorable: false,
    createdAt: s.iso(f.createdAt),
    updatedAt: s.iso(f.createdAt),
    url: "/" + a.login,
  };
}

const dayKey = (ms: number) => new Date(ms).toISOString().slice(0, 10);

/** Repos whose commits count toward someone's graph: public ones, plus the viewer's own private work. */
function contributionRepos(s: FixtureStore, viewer: string | undefined, a: Account): RepoState[] {
  return [...s.repos.values()].filter((r) => r.repo.visibility === "VISIBILITY_PUBLIC" || (viewer !== undefined && viewer === a.handle && s.canRead(viewer, r)));
}

function contributionGraph(s: FixtureStore, viewer: string | undefined, a: Account) {
  const real = new Map<string, number>();
  for (const r of contributionRepos(s, viewer, a)) for (const c of r.main) if (c.author === a.handle) real.set(dayKey(c.time), (real.get(dayKey(c.time)) ?? 0) + 1);
  const end = Date.UTC(new Date(s.anchor).getUTCFullYear(), new Date(s.anchor).getUTCMonth(), new Date(s.anchor).getUTCDate());
  const rnd = prng(fnv1a("graph:" + a.login));
  const busy = 0.25 + (fnv1a("busy:" + a.login) % 50) / 100;
  const days: { date: string; count: number }[] = [];
  for (let i = 364; i >= 0; i--) {
    const at = end - i * DAY;
    const roll = rnd();
    const size = rnd();
    const dow = new Date(at).getUTCDay();
    const noise = roll < (dow === 0 || dow === 6 ? busy * 0.35 : busy) ? 1 + Math.floor(size * size * 9) : 0;
    const date = dayKey(at);
    days.push({ date, count: noise + (real.get(date) ?? 0) * 2 });
  }
  const nonZero = days.map((d) => d.count).filter((n) => n > 0).sort((x, y) => x - y);
  const q = (p: number) => nonZero[Math.floor((nonZero.length - 1) * p)] ?? 0;
  const [q1, q2, q3] = [q(0.25), q(0.5), q(0.75)];
  const level = (n: number) => (n === 0 ? 0 : n <= q1 ? 1 : n <= q2 ? 2 : n <= q3 ? 3 : 4);
  return {
    login: a.login,
    fromDate: days[0]?.date ?? "",
    toDate: days[days.length - 1]?.date ?? "",
    total: days.reduce((n, d) => n + d.count, 0),
    days: days.map((d) => ({ date: d.date, count: d.count, level: level(d.count) })),
    includesPrivate: viewer !== undefined && viewer === a.handle,
  };
}

const tier = (n: number, silver: number, gold: number) => (n >= gold ? "gold" : n >= silver ? "silver" : "bronze");

function achievements(s: FixtureStore, viewer: string | undefined, a: Account): UserAchievementJson[] {
  const handle = a.handle ?? "";
  const repos = contributionRepos(s, viewer, a);
  const mine = repos.flatMap((r) => r.main.filter((c) => c.author === handle).map((c) => ({ r, c }))).sort((x, y) => x.c.time - y.c.time || x.c.idText.localeCompare(y.c.idText));
  const out: UserAchievementJson[] = [];
  const first = mine[0];
  if (first)
    out.push({
      slug: "first-land",
      name: "First land",
      description: "Landed a change on main.",
      tier: tier(mine.length, 5, 15),
      count: mine.length,
      imageUrl: "",
      unlockedBy: { kind: "SUBJECT_KIND_CHANGESET", repo: { fullName: first.r.repo.fullName }, id: first.c.idText },
      unlockedAt: s.iso(first.c.time),
    });
  const touched = [...new Set(mine.map((x) => x.r))];
  if (touched.length > 0)
    out.push({
      slug: "cross-repo",
      name: "Cross-repo contributor",
      description: "Landed work in more than one repository.",
      tier: tier(touched.length, 3, 6),
      count: touched.length,
      imageUrl: "",
      unlockedBy: { kind: "SUBJECT_KIND_REPO", repo: { fullName: touched[0]?.repo.fullName }, id: touched[0]?.repo.id },
      unlockedAt: s.iso(first?.c.time ?? s.anchor),
    });
  const keys = new Set(repos.map((r) => r.name));
  const reviews = s.crs
    .filter((c) => keys.has(c.repo))
    .flatMap((c) => c.reviews.filter((rv) => s.handleOf(rv.reviewer?.id ?? "") === handle).map((rv) => ({ c, rv })))
    .sort((x, y) => Date.parse(x.rv.createdAt ?? "") - Date.parse(y.rv.createdAt ?? "") || x.rv.id.localeCompare(y.rv.id));
  const firstReview = reviews[0];
  if (firstReview)
    out.push({
      slug: "reviewer",
      name: "Code reviewer",
      description: "Reviewed other people's changes.",
      tier: tier(reviews.length, 5, 15),
      count: reviews.length,
      imageUrl: "",
      unlockedBy: { kind: "SUBJECT_KIND_CHANGE_REQUEST", repo: { fullName: s.repos.get(firstReview.c.repo)?.repo.fullName }, id: firstReview.c.id },
      unlockedAt: firstReview.rv.createdAt,
    });
  const agents = [...s.principals.entries()].filter(([, p]) => p.kind === "PRINCIPAL_KIND_AGENT" && s.handleOf(p.ownerId) === handle);
  if (agents.length)
    out.push({ slug: "agent-owner", name: "Agent owner", description: "Runs agents that land changes.", tier: tier(agents.length, 2, 4), count: agents.length, imageUrl: "", unlockedAt: s.iso(a.handle ? profileFacts(s, a).createdAt : s.anchor) });
  const starred = starCountOf(s, a.login);
  if (starred > 0 && a.kind !== "org")
    out.push({ slug: "stargazer", name: "Stargazer", description: "Starred repositories.", tier: tier(starred, 5, 20), count: starred, imageUrl: "", unlockedAt: s.iso(profileFacts(s, a).createdAt) });
  return out;
}

interface FeedItem {
  at: number;
  key: string;
  kind: string;
  actor: string;
  repo?: RepoState;
  subject: UserFeedEventJson["subject"];
  summary: string;
  payload: unknown;
}

/** The viewer's home feed: activity in the repos they work in or starred, and what the people they follow did. */
function feed(s: FixtureStore, viewer: string): FeedItem[] {
  const login = loginOf(s, viewer);
  const readable = s.readableRepos(viewer);
  const starred = new Set([...s.stars].filter(([, rows]) => rows.some((r) => lower(r.who) === lower(login))).map(([k]) => k));
  const mine = new Set(readable.filter((r) => s.inOrg(viewer, r.owner) || starred.has(r.name)).map((r) => r.name));
  const followed = new Set(s.follows.filter((f) => lower(f.from) === lower(login)).map((f) => lower(f.to)));
  const followedHandle = (h: string) => followed.has(lower(loginOf(s, h)));
  const items: FeedItem[] = [];
  const name = (h: string) => s.p(h).display;
  for (const r of readable) {
    for (const c of r.main) {
      if (isService(s, c.author) || !(mine.has(r.name) || followedHandle(c.author))) continue;
      items.push({
        at: c.time,
        key: "land:" + c.idText,
        kind: "land",
        actor: c.author,
        repo: r,
        subject: { kind: "SUBJECT_KIND_CHANGESET", repo: { fullName: r.repo.fullName }, id: c.idText },
        summary: name(c.author) + ' landed "' + c.message + '" in ' + r.repo.fullName,
        payload: { message: c.message, changeset: c.idText },
      });
    }
  }
  for (const c of s.crs) {
    const r = s.repos.get(c.repo);
    if (!r || !readable.includes(r)) continue;
    for (const rv of c.reviews) {
      const who = s.handleOf(rv.reviewer?.id ?? "");
      if (isService(s, who) || !(mine.has(r.name) || followedHandle(who))) continue;
      const verb = rv.verdict === "VERDICT_APPROVE" ? "approved" : rv.verdict === "VERDICT_REQUEST_CHANGES" ? "requested changes on" : "commented on";
      items.push({
        at: Date.parse(rv.createdAt ?? ""),
        key: "review:" + rv.id,
        kind: "review",
        actor: who,
        repo: r,
        subject: { kind: "SUBJECT_KIND_CHANGE_REQUEST", repo: { fullName: r.repo.fullName }, id: c.id },
        summary: name(who) + " " + verb + " #" + c.number + ' "' + c.title + '" in ' + r.repo.fullName,
        payload: { verdict: rv.verdict, number: c.number, title: c.title },
      });
    }
  }
  for (const rel of s.releases) {
    const r = [...s.repos.values()].find((x) => x.repo.id === rel.repoId);
    if (!r || !mine.has(r.name)) continue;
    const who = s.handleOf(rel.author?.id ?? "");
    items.push({
      at: Date.parse(rel.createdAt ?? ""),
      key: "release:" + rel.id,
      kind: "release",
      actor: who,
      repo: r,
      subject: { kind: "SUBJECT_KIND_RELEASE", repo: { fullName: r.repo.fullName }, id: rel.id },
      summary: name(who) + " released " + rel.tag + " of " + r.repo.fullName,
      payload: { tag: rel.tag },
    });
  }
  for (const [key, rows] of s.stars) {
    const r = s.repos.get(key);
    if (!r || !readable.includes(r)) continue;
    for (const row of rows) {
      const h = s.handleByLogin.get(lower(row.who));
      if (!h || lower(row.who) === lower(login) || !(followed.has(lower(row.who)) || mine.has(key))) continue;
      items.push({
        at: row.at,
        key: "star:" + key + ":" + row.who,
        kind: "star",
        actor: h,
        repo: r,
        subject: { kind: "SUBJECT_KIND_REPO", repo: { fullName: r.repo.fullName }, id: r.repo.id },
        summary: name(h) + " starred " + r.repo.fullName,
        payload: { repo: r.repo.fullName },
      });
    }
  }
  for (const f of s.follows) {
    const h = s.handleByLogin.get(lower(f.from));
    if (!h || lower(f.from) === lower(login) || !(lower(f.to) === lower(login) || followed.has(lower(f.from)))) continue;
    const target = accountByLogin(s, f.to);
    items.push({
      at: f.at,
      key: "follow:" + f.from + ">" + f.to,
      kind: "follow",
      actor: h,
      subject: { kind: "SUBJECT_KIND_USER", id: f.to },
      summary: name(h) + " followed " + (target ? displayName(s, target) : f.to),
      payload: { login: f.to },
    });
  }
  return items.sort((x, y) => y.at - x.at || x.key.localeCompare(y.key));
}

const users = defineService(UserService, {
  getViewer: (s, _req, ctx) => {
    const viewer = me(s, ctx);
    return profileJson(s, viewer, userAccount(s, { id: s.p(viewer).id }));
  },
  ...repoFollowImpl,
  getUser: (s, req, ctx) => {
    if (!req.login && !req.id) throw invalid("pass a login or an id", "INVALID_ARGUMENT");
    return profileJson(s, s.viewer(ctx.token), userAccount(s, req));
  },

  starRepo: (s, req, ctx) => {
    const login = loginOf(s, me(s, ctx));
    const repo = s.repo(req.repo);
    const rows = s.stars.get(repo.name) ?? [];
    if (!rows.some((r) => lower(r.who) === lower(login))) rows.push({ who: login, at: s.now() });
    s.stars.set(repo.name, rows);
    return {};
  },
  unstarRepo: (s, req, ctx) => {
    const login = loginOf(s, me(s, ctx));
    const repo = s.repo(req.repo);
    s.stars.set(repo.name, (s.stars.get(repo.name) ?? []).filter((r) => lower(r.who) !== lower(login)));
    return {};
  },
  isRepoStarred: (s, req, ctx) => {
    const login = loginOf(s, me(s, ctx));
    return { starred: (s.stars.get(s.repo(req.repo).name) ?? []).some((r) => lower(r.who) === lower(login)), starListIds: [] };
  },
  listRepoStargazers: (s, req) => {
    const rows = s.stars.get(s.repo(req.repo).name) ?? [];
    const result = page(rows, req.page);
    return {
      stargazers: result.items.flatMap(({ who, at }) => {
        const account = accountByLogin(s, who);
        return account ? [{ user: summary(s, account), starredAt: s.iso(at) }] : [];
      }),
      totalCount: String(rows.length),
      page: { nextPageToken: result.next },
    };
  },
  listStarredRepos: (s, req, ctx) => {
    const viewer = s.viewer(ctx.token);
    if (!req.login && !viewer) throw invalid("pass a login, or sign in to list your own stars", "INVALID_ARGUMENT");
    const who = req.login ? userAccount(s, { login: req.login }) : userAccount(s, { login: loginOf(s, viewer as string) });
    const lang = lower(req.language ?? "");
    const rows: { r: RepoState; at: number }[] = [];
    for (const [key, stars] of s.stars) {
      const r = s.repos.get(key);
      const mine = stars.find((x) => lower(x.who) === lower(who.login));
      if (r && mine && s.canRead(viewer, r) && (!lang || lower(s.repoView(r).primaryLanguage) === lang)) rows.push({ r, at: mine.at });
    }
    const count = (r: RepoState) => s.stars.get(r.name)?.length ?? 0;
    const sort = lower(req.sort ?? "");
    const pushed = (r: RepoState) => r.refs.get(r.repo.defaultRef)?.target.time ?? 0;
    rows.sort((a, b) => {
      const by = sort === "stars" ? count(b.r) - count(a.r) : sort === "updated" ? pushed(b.r) - pushed(a.r) : sort === "name" ? a.r.repo.fullName.localeCompare(b.r.repo.fullName) : b.at - a.at;
      return by || a.r.repo.fullName.localeCompare(b.r.repo.fullName);
    });
    const pg = page(rows, req.page, 30);
    return {
      repos: pg.items.map(({ r, at }) => {
        const v = s.repoView(r);
        return { repo: { fullName: r.repo.fullName }, fullName: r.repo.fullName, description: r.repo.description, language: v.primaryLanguage, starCount: v.stars, starredAt: s.iso(at) };
      }),
      page: { nextPageToken: pg.next },
    };
  },

  followUser: (s, req, ctx) => {
    const from = loginOf(s, me(s, ctx));
    const target = accountByLogin(s, req.login);
    if (!target) throw userNotFound(req.login ?? "");
    if (lower(target.login) === lower(from)) throw invalid("you cannot follow yourself", "CANNOT_FOLLOW_SELF");
    if (!isFollowing(s, from, target.login)) s.follows.push({ from, to: target.login, at: s.now() });
    return {};
  },
  unfollowUser: (s, req, ctx) => {
    const from = loginOf(s, me(s, ctx));
    const target = accountByLogin(s, req.login);
    if (!target) throw userNotFound(req.login ?? "");
    s.follows = s.follows.filter((f) => !(lower(f.from) === lower(from) && lower(f.to) === lower(target.login)));
    return {};
  },
  listUserFollowers: (s, req) => {
    const target = accountByLogin(s, req.login);
    if (!target) throw userNotFound(req.login ?? "");
    const rows = s.follows.filter((f) => lower(f.to) === lower(target.login)).sort((a, b) => b.at - a.at || a.from.localeCompare(b.from));
    const pg = page(rows, req.page, 30);
    return { users: pg.items.flatMap((f) => accountByLogin(s, f.from) ?? []).map((a) => summary(s, a)), page: { nextPageToken: pg.next } };
  },
  listUserFollowing: (s, req) => {
    const target = accountByLogin(s, req.login);
    if (!target) throw userNotFound(req.login ?? "");
    const rows = s.follows.filter((f) => lower(f.from) === lower(target.login)).sort((a, b) => b.at - a.at || a.to.localeCompare(b.to));
    const pg = page(rows, req.page, 30);
    return { users: pg.items.flatMap((f) => accountByLogin(s, f.to) ?? []).map((a) => summary(s, a)), page: { nextPageToken: pg.next } };
  },

  getUserContributionGraph: (s, req, ctx) => contributionGraph(s, s.viewer(ctx.token), userAccount(s, { login: req.login })),
  listUserAchievements: (s, req, ctx) => ({ achievements: achievements(s, s.viewer(ctx.token), userAccount(s, { login: req.login })) }),

  listUserFeedEvents: (s, req, ctx) => {
    const kinds = new Set(req.kinds ?? []);
    const items = feed(s, me(s, ctx)).filter((i) => !kinds.size || kinds.has(i.kind));
    const pg = page(items, req.page, 30);
    return {
      events: pg.items.map((i) => ({
        id: ulid(i.at, i.key),
        actor: s.p(i.actor),
        kind: i.kind,
        subject: i.subject,
        summary: i.summary,
        payload: JSON.stringify(i.payload),
        createdAt: s.iso(i.at),
      })),
      page: { nextPageToken: pg.next },
    };
  },
});

// ------------------------------------------------------------------ insights

const insights = defineService(InsightsService, {
  getLanguageStats: (s, req) => {
    const repo = s.repo(req.repo);
    if (!repo.main.length) return { languages: [], totalBytes: "0" };
    const at = s.resolve(repo, req.at?.expr ?? "");
    const stats = s.languagesAt(at, { includeDocumentation: req.includeDocumentation, includeGenerated: req.includeGenerated });
    return { languages: stats.languages, totalBytes: String(stats.totalBytes) };
  },
});

export const COMMUNITY_SERVICES: ServiceDef[] = [users, insights, discovery];
