/**
 * Community and discovery: repo home, profiles, stars and follows, the home feed, search,
 * trending, leaderboards and topics (users.proto, search.proto, insights.proto, vcs.proto).
 *
 * The anonymous server answers a short list of read RPCs (`ANONYMOUS_READ_METHODS`). Pages that
 * signed-out visitors can open (repo home, profiles) therefore treat `unauthenticated` and
 * `permission_denied` from the signed-in-only parts as "not shown to you", and nothing else:
 * `unimplemented`, `unavailable` and every other error reach the screen as they are.
 */
import { InsightsService } from "../../gen/bee/v1/insights_pb";
import { OrgService, RepoService, LeaseService } from "../../gen/bee/v1/forge_pb";
import { SearchService } from "../../gen/bee/v1/search_pb";
import { UserService } from "../../gen/bee/v1/users_pb";
import { ObjectService, QueryService } from "../../gen/bee/v1/vcs_pb";
import { base64ToBytes, pathBytes } from "../ids";
import type { Rpc } from "../rpc";
import { BeeError, type CallOptions } from "../transport";
import type * as T from "../types";
import { withHolders } from "./forge";
import { clean, list, mapLimit, num, pass, str, wire, type PartialOps, type UnaryOp } from "./shared";

/** The caller is not allowed to see this part; the rest of the page still renders. */
function hiddenFromCaller(err: unknown): boolean {
  return err instanceof BeeError && (err.code === "unauthenticated" || err.code === "permission_denied");
}

async function orEmpty<X>(p: Promise<X>, empty: X): Promise<X> {
  try {
    return await p;
  } catch (err) {
    if (hiddenFromCaller(err)) return empty;
    throw err;
  }
}

/** Reads at most `max` bytes of a file as text. `undefined` when the path is missing or binary. */
async function readFile(rpc: Rpc, repo: T.RepoRef, ref: string, path: string, max: number, opts?: CallOptions): Promise<{ text: string; truncated: boolean } | undefined> {
  const parts: Uint8Array[] = [];
  let got = 0;
  let size = 0;
  try {
    for await (const chunk of rpc.stream(QueryService.method.readPath, clean({ repo, at: { expr: ref }, path: pathBytes(path) }), opts)) {
      const data = base64ToBytes(str(chunk.data));
      parts.push(data);
      got += data.length;
      size = num(chunk.size);
      if (got > max) break;
    }
  } catch (err) {
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "failed_precondition")) return undefined;
    throw err;
  }
  const all = new Uint8Array(got);
  let o = 0;
  for (const p of parts) {
    all.set(p, o);
    o += p.length;
  }
  if (all.includes(0)) return undefined;
  const cut = all.subarray(0, Math.min(max, all.length));
  return { text: new TextDecoder().decode(cut), truncated: got > max || size > max };
}

const README_BYTES = 256 * 1024;
const README_NAMES = ["README.md", "README", "readme.md", "README.markdown"];

async function readme(rpc: Rpc, repo: T.RepoRef, ref: string, opts?: CallOptions): Promise<T.ReadmeFile | undefined> {
  for (const name of README_NAMES) {
    const f = await readFile(rpc, repo, ref, name, README_BYTES, opts);
    if (f) return { path: name, markdown: f.text, truncated: f.truncated };
  }
  return undefined;
}

/** Where a clone starts: the server's public base URL plus `<owner>/<repo>.git`. Empty when the server does not say. */
export function cloneUrls(info: { connectBaseUrl?: string; features?: string[] }, fullName: string): T.CloneUrls {
  const base = str(info.connectBaseUrl).replace(/\/+$/, "");
  if (!base) return { https: "" };
  const host = base.replace(/^[a-z]+:\/\//, "").replace(/\/.*$/, "");
  return { https: base + "/" + fullName + ".git", ...(list(info.features).includes("ssh") ? { ssh: "git@" + host + ":" + fullName + ".git" } : {}) };
}

async function repoHome(rpc: Rpc, r: T.GetRepoHomeRequest, opts?: CallOptions): Promise<T.RepoHome> {
  const ref: T.RepoRef = { fullName: r.fullName };
  const [repo, info] = await Promise.all([rpc.call(RepoService.method.getRepo, { repo: ref }, opts), rpc.call(ObjectService.method.getServerInfo, {}, opts)]);
  const at = str(repo.defaultRef) || "main";
  const [file, languages, log, leases, starred] = await Promise.all([
    readme(rpc, ref, at, opts),
    orEmpty(rpc.call(InsightsService.method.getLanguageStats, { repo: ref, at: { expr: at } }, opts), undefined),
    latestLand(rpc, ref, at, opts),
    orEmpty<{ leases?: T.Lease[] } | undefined>(rpc.call(LeaseService.method.listLeases, { repo: ref }, opts) as Promise<{ leases?: T.Lease[] }>, undefined),
    orEmpty(rpc.call(UserService.method.isRepoStarred, { repo: ref }, opts), undefined),
  ]);
  return {
    repo: repo as unknown as T.Repo,
    readme: file,
    languages: list(languages?.languages).map((l) => ({ name: str(l.name), byteCount: str(l.byteCount) || "0", fileCount: num(l.fileCount), percentage: num(l.percentage), color: str(l.color) })),
    latestLand: log,
    openChanges: num(repo.openChanges),
    leases: leases ? await withHolders(rpc, list(leases.leases), opts) : [],
    leasesVisible: leases !== undefined,
    cloneUrls: cloneUrls(info, str(repo.fullName) || r.fullName),
    starredByMe: !!starred?.starred,
  };
}

async function latestLand(rpc: Rpc, repo: T.RepoRef, at: string, opts?: CallOptions): Promise<T.Changeset | undefined> {
  try {
    const log = await rpc.call(QueryService.method.log, clean({ repo, from: { expr: at }, firstParent: true, includeMeta: true, page: { pageSize: 1 } }), opts);
    return list(log.changesets)[0] as unknown as T.Changeset | undefined;
  } catch (err) {
    // A repo with nothing landed yet has no tip to read.
    if (err instanceof BeeError && (err.code === "not_found" || err.code === "failed_precondition")) return undefined;
    throw err;
  }
}

async function listMyRepos(rpc: Rpc, r: T.ListMyReposRequest, opts?: CallOptions): Promise<T.ListMyReposResponse> {
  const orgs = list((await rpc.call(OrgService.method.listOrgs, clean({ page: r.page ?? { pageSize: 100 } }), opts)).orgs);
  const per = await mapLimit(orgs, 6, (o) => rpc.call(RepoService.method.listRepos, { org: str(o.name), page: { pageSize: 200 } }, opts));
  const repos = per.flatMap((x) => list(x.repos)) as unknown as T.Repo[];
  // Newest push first, then name, so the order is the same on every call.
  repos.sort((a, b) => (Date.parse(b.pushedAt ?? "") || 0) - (Date.parse(a.pushedAt ?? "") || 0) || a.fullName.localeCompare(b.fullName));
  return { repos };
}

async function publicActivity(rpc: Rpc, r: T.ListPublicActivityRequest, opts?: CallOptions): Promise<T.ListPublicActivityResponse> {
  const limit = Math.max(1, Math.min(20, r.limit ?? 20));
  const found = await rpc.call(SearchService.method.searchRepos, { query: "is:public sort:updated", sort: "updated", page: { pageSize: limit } }, opts);
  const hits = list(found.hits).slice(0, limit) as unknown as T.SearchRepoHit[];
  const lands = await Promise.all(hits.map((h) => latestLand(rpc, { fullName: h.fullName }, "main", opts)));
  const items: T.PublicLand[] = [];
  hits.forEach((repo, i) => {
    const changeset = lands[i];
    if (changeset) items.push({ repo, changeset });
  });
  items.sort((a, b) => Date.parse(b.changeset.meta?.committerTime?.time ?? "") - Date.parse(a.changeset.meta?.committerTime?.time ?? "") || a.repo.fullName.localeCompare(b.repo.fullName));
  return { items };
}

async function profile(rpc: Rpc, r: T.GetProfileRequest, opts?: CallOptions): Promise<T.Profile> {
  try {
    const u = await rpc.call(UserService.method.getUser, { login: r.owner }, opts);
    const kind = u.principal?.kind === "PRINCIPAL_KIND_AGENT" ? "agent" : "user";
    const owner = kind === "agent" && u.principal?.ownerId ? await rpc.call(UserService.method.getUser, { id: u.principal.ownerId }, opts).catch(() => undefined) : undefined;
    return {
      kind,
      principalId: str(u.principal?.id),
      login: str(u.login),
      name: str(u.name),
      bio: str(u.bio),
      avatar: u.avatarUrl || undefined,
      readme: u.readmeHtml || undefined,
      company: str(u.company),
      location: str(u.location),
      website: str(u.website),
      orgs: list(u.orgs),
      followers: num(u.followerCount),
      following: num(u.followingCount),
      followingByMe: !!u.viewerIsFollowing,
      publicRepos: num(u.publicRepoCount),
      ...(kind === "agent" ? { runtime: str(u.principal?.runtime), ownerLogin: owner?.login || undefined } : {}),
      createdAt: u.createdAt,
    };
  } catch (err) {
    if (!(err instanceof BeeError) || err.code !== "not_found") throw err;
  }
  // Not a user: an org is found by search, which is also open to signed-out visitors.
  const found = await rpc.call(SearchService.method.searchUsers, { query: "type:org in:login " + r.owner, page: { pageSize: 10 } }, opts);
  const org = list(found.hits).find((h) => str(h.login).toLowerCase() === r.owner.toLowerCase() && h.isOrg);
  if (!org) throw new BeeError("not_found", { reason: "USER_NOT_FOUND", message: "no user or organization named " + r.owner });
  return {
    kind: "org",
    principalId: str(org.principal?.id),
    login: str(org.login),
    name: str(org.name),
    bio: str(org.bio),
    avatar: org.avatarUrl || undefined,
    company: "",
    location: str(org.location),
    website: "",
    orgs: [],
    followers: num(org.followerCount),
    following: 0,
    followingByMe: false,
    publicRepos: num(org.publicRepoCount),
  };
}

export const communityOps = {
  OrgService: {
    CreateOrg: pass(OrgService.method.createOrg),
  },
  RepoService: {
    ListMyRepos: {
      wire: () => wire(OrgService.method.listOrgs, {}),
      run: (rpc, r, opts) => listMyRepos(rpc, r, opts),
    } satisfies UnaryOp<T.ListMyReposRequest, T.ListMyReposResponse>,
    GetRepoHome: {
      wire: (r) => wire(RepoService.method.getRepo, { repo: { fullName: r.fullName } }),
      run: (rpc, r, opts) => repoHome(rpc, r, opts),
    } satisfies UnaryOp<T.GetRepoHomeRequest, T.RepoHome>,
    CreateRepo: pass(RepoService.method.createRepo),
  },
  ObjectService: {
    GetServerInfo: pass(ObjectService.method.getServerInfo),
  },
  InsightsService: {
    GetLanguages: pass(InsightsService.method.getLanguageStats),
  },
  SearchService: {
    SearchRepos: pass(SearchService.method.searchRepos),
    ListTrendingRepos: pass(SearchService.method.listTrendingRepos),
    ListTrendingDevelopers: pass(SearchService.method.listTrendingDevelopers),
    ListLeaderboard: pass(SearchService.method.listLeaderboard),
    GetTopic: pass(SearchService.method.getTopic),
    ListFeaturedTopics: pass(SearchService.method.listFeaturedTopics),
    ListPublicActivity: {
      wire: (r) => wire(SearchService.method.searchRepos, { query: "is:public sort:updated", sort: "updated", page: { pageSize: Math.max(1, Math.min(20, r.limit ?? 20)) } }),
      run: (rpc, r, opts) => publicActivity(rpc, r, opts),
    } satisfies UnaryOp<T.ListPublicActivityRequest, T.ListPublicActivityResponse>,
  },
  UserService: {
    GetProfile: {
      wire: (r) => wire(UserService.method.getUser, { login: r.owner }),
      run: (rpc, r, opts) => profile(rpc, r, opts),
    } satisfies UnaryOp<T.GetProfileRequest, T.Profile>,
    StarRepo: pass(UserService.method.starRepo),
    UnstarRepo: pass(UserService.method.unstarRepo),
    ListStarred: pass(UserService.method.listStarredRepos),
    FollowUser: pass(UserService.method.followUser),
    UnfollowUser: pass(UserService.method.unfollowUser),
    ListFollowers: pass(UserService.method.listUserFollowers),
    ListFollowing: pass(UserService.method.listUserFollowing),
    GetContributionGraph: pass(UserService.method.getUserContributionGraph),
    ListAchievements: pass(UserService.method.listUserAchievements),
    ListHomeFeed: pass(UserService.method.listUserFeedEvents),
  },
} satisfies PartialOps<"OrgService" | "RepoService" | "ObjectService" | "InsightsService" | "SearchService" | "UserService">;
