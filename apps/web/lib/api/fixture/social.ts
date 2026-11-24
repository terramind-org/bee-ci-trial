/**
 * Shared reads over the fixture world for the community handlers: accounts (users, agents and
 * orgs), the counters behind profiles, and the windowed counts behind trending and leaderboards.
 * Everything is a pure function of the store and the viewer.
 */
import type { PrincipalJson } from "../../gen/bee/v1/common_pb";
import type { SearchRepoHitJson, SearchUserHitJson } from "../../gen/bee/v1/search_pb";
import type { UserSummaryJson } from "../../gen/bee/v1/users_pb";
import { BeeError } from "../transport";
import { PROFILES, type ProfileSeed } from "./community-world";
import { fnv1a } from "./seed";
import type { FixtureStore, OrgRecord, RepoState } from "./store";

export const DAY = 86_400_000;

export type AccountKind = "user" | "agent" | "org";

export interface Account {
  kind: AccountKind;
  login: string;
  /** Principal handle; absent for orgs. */
  handle?: string;
  org?: OrgRecord;
}

export function userNotFound(login: string): BeeError {
  return new BeeError("not_found", { reason: "USER_NOT_FOUND", message: "no user or agent named " + login });
}

/** Humans, then agents, then orgs, in world order. */
export function accounts(s: FixtureStore): Account[] {
  const out: Account[] = [];
  for (const [handle, pr] of s.principals) {
    const login = s.loginByHandle.get(handle);
    if (!login) continue;
    out.push({ kind: pr.kind === "PRINCIPAL_KIND_AGENT" ? "agent" : "user", login, handle });
  }
  for (const org of s.orgs.values()) out.push({ kind: "org", login: org.org.name, org });
  return out;
}

export function accountByLogin(s: FixtureStore, login: string | undefined): Account | undefined {
  const l = (login ?? "").toLowerCase();
  if (!l) return undefined;
  return accounts(s).find((a) => a.login.toLowerCase() === l);
}

/** A person or an agent (not an org) by login or principal id. */
export function userAccount(s: FixtureStore, req: { login?: string; id?: string }): Account {
  const handle = req.login ? s.handleByLogin.get(req.login.toLowerCase()) : req.id ? s.byId.get(req.id) : undefined;
  const login = handle ? s.loginByHandle.get(handle) : undefined;
  if (!handle || !login) throw userNotFound(req.login || req.id || "");
  return { kind: s.kindOf(handle) === "agent" ? "agent" : "user", login, handle };
}

export function loginOf(s: FixtureStore, handle: string): string {
  return s.loginByHandle.get(handle) ?? handle;
}

export function principalOf(s: FixtureStore, a: Account): PrincipalJson {
  if (a.handle) return s.p(a.handle);
  const org = a.org?.org;
  return { id: org?.id ?? "", kind: "PRINCIPAL_KIND_UNSPECIFIED", display: org?.display ?? a.login, ownerId: "", runtime: "", externalIds: [] };
}

export function displayName(s: FixtureStore, a: Account): string {
  return a.org ? a.org.org.display : s.p(a.handle ?? "").display;
}

export interface ProfileFacts extends ProfileSeed {
  createdAt: number;
}

export function profileFacts(s: FixtureStore, a: Account): ProfileFacts {
  if (a.org) {
    return { bio: a.org.description, company: "", location: a.org.location, website: a.org.website, createdAt: Date.parse(a.org.org.createdAt ?? "") };
  }
  const handle = a.handle ?? "";
  const known = PROFILES[handle];
  const owner = s.p(handle).ownerId ? s.handleOf(s.p(handle).ownerId) : "";
  const home = [...s.orgs.values()].find((o) => s.inOrg(handle, o.org.name));
  const base: ProfileSeed = known ?? { bio: s.agentMeta.get(handle)?.purpose ?? "", company: owner ? (home?.org.display ?? "") : "", location: "", website: "" };
  return { ...base, createdAt: s.anchor - (120 + (fnv1a("joined:" + a.login) % 180)) * DAY };
}

// ------------------------------------------------------------------ counters

const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function followerCount(s: FixtureStore, login: string): number {
  return s.follows.filter((f) => same(f.to, login)).length;
}
export function followingCount(s: FixtureStore, login: string): number {
  return s.follows.filter((f) => same(f.from, login)).length;
}
export function isFollowing(s: FixtureStore, from: string | undefined, to: string): boolean {
  return !!from && s.follows.some((f) => same(f.from, from) && same(f.to, to));
}
export function starCountOf(s: FixtureStore, login: string): number {
  let n = 0;
  for (const rows of s.stars.values()) n += rows.filter((r) => same(r.who, login)).length;
  return n;
}

export const isService = (s: FixtureStore, handle: string) => s.kindOf(handle) === "service";

/** Public repos an account owns (orgs) or has landed work in (people and agents). */
export function publicRepoCount(s: FixtureStore, a: Account): number {
  const publics = [...s.repos.values()].filter((r) => r.repo.visibility === "VISIBILITY_PUBLIC");
  if (a.org) return publics.filter((r) => r.owner === a.org?.org.name).length;
  return publics.filter((r) => r.main.some((c) => c.author === a.handle)).length;
}

export function visibilityWord(r: RepoState): string {
  return r.repo.visibility === "VISIBILITY_PUBLIC" ? "public" : r.repo.visibility === "VISIBILITY_INTERNAL" ? "internal" : "private";
}

// ------------------------------------------------------------------ hits

export function repoHit(s: FixtureStore, r: RepoState, score = 0): SearchRepoHitJson {
  const v = s.repoView(r);
  return {
    repo: { fullName: r.repo.fullName },
    fullName: r.repo.fullName,
    description: r.repo.description,
    topics: v.topics,
    primaryLanguage: v.primaryLanguage,
    starCount: v.stars,
    forkCount: v.forks,
    openIssueCount: v.openIssues,
    licenseSpdxId: v.licenseSpdx,
    archived: v.archived,
    isFork: false,
    isTemplate: false,
    visibility: visibilityWord(r),
    updatedAt: v.pushedAt,
    pushedAt: v.pushedAt,
    score,
  };
}

export function userHit(s: FixtureStore, a: Account): SearchUserHitJson {
  const f = profileFacts(s, a);
  return {
    principal: principalOf(s, a),
    login: a.login,
    name: displayName(s, a),
    isOrg: a.kind === "org",
    bio: f.bio,
    location: f.location,
    avatarUrl: "",
    followerCount: String(followerCount(s, a.login)),
    publicRepoCount: String(publicRepoCount(s, a)),
  };
}

export function summary(s: FixtureStore, a: Account): UserSummaryJson {
  return { principal: principalOf(s, a), login: a.login, name: displayName(s, a), avatarUrl: "", bio: profileFacts(s, a).bio };
}

// ------------------------------------------------------------------ windows

export type WindowName = "TRENDING_WINDOW_DAILY" | "TRENDING_WINDOW_WEEKLY" | "TRENDING_WINDOW_MONTHLY" | "TRENDING_WINDOW_UNSPECIFIED" | undefined;

/** Window length in ms; an unset window is daily, as on the server. */
export function windowMs(w: WindowName): number {
  return w === "TRENDING_WINDOW_WEEKLY" ? 7 * DAY : w === "TRENDING_WINDOW_MONTHLY" ? 30 * DAY : DAY;
}

/** Lands on main since `since`, by author handle; service accounts do not count. */
export function landsByAuthor(s: FixtureStore, r: RepoState, since: number): Map<string, number> {
  const out = new Map<string, number>();
  for (const c of r.main) if (c.time > since && !isService(s, c.author)) out.set(c.author, (out.get(c.author) ?? 0) + 1);
  return out;
}

export function landsIn(s: FixtureStore, r: RepoState, since: number): number {
  let n = 0;
  for (const v of landsByAuthor(s, r, since).values()) n += v;
  return n;
}

export function starsIn(s: FixtureStore, r: RepoState, since: number): number {
  return (s.stars.get(r.name) ?? []).filter((x) => x.at > since).length;
}

/** Reviews submitted since `since` on the given repos, by reviewer handle. */
export function reviewsByReviewer(s: FixtureStore, repos: RepoState[], since: number): Map<string, number> {
  const keys = new Set(repos.map((r) => r.name));
  const out = new Map<string, number>();
  for (const c of s.crs) {
    if (!keys.has(c.repo)) continue;
    for (const rv of c.reviews) {
      const who = s.handleOf(rv.reviewer?.id ?? "");
      if (Date.parse(rv.createdAt ?? "") > since && !isService(s, who)) out.set(who, (out.get(who) ?? 0) + 1);
    }
  }
  return out;
}
